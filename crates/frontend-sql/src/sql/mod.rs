//! SQL → the canonical, unresolved
//! [`UnresolvedQueryExpr`](asap_types::pre_asap::query_expr::UnresolvedQueryExpr)
//! (`QueryExpr<ColumnRef>`).
//!
//! Parses SQL via DataFusion (over the catalog's registered tables), then
//! walks the unoptimized `LogicalPlan` and emits `UnresolvedQueryExpr` nodes with
//! unresolved `ColumnRef`s directly (issue #179) — the same tree shape
//! [`resolve_root`](asap_types::pre_asap::resolve_root) binds to canonical,
//! positional `QueryExpr<ColumnId>`. Unlike PromQL's front end, SQL's
//! `Aggregate` nodes need no reduction-shape decision at construction time —
//! DataFusion's `Aggregate` plan node is always `Reduction::Reduce`, never
//! PromQL's per-series `PerEntity` (there is no windowed/subquery child
//! concept in SQL) — so this front end always builds `Reduce` directly. It
//! does still have to fold a `WHERE` directly over a bare table scan onto
//! `Scan.predicates` itself (`filter_or_fold`) — canonical's invariant that a
//! `Filter` never sits directly over a `Scan` — since front ends producing
//! this shape are responsible for it now, not a converter.
//!
//! Heavy-hitter `topk` recognition (`ORDER BY count(...) DESC LIMIT k`) is
//! *not* done here: SQL emits a plain `Sort`/`Limit`, and the shared
//! `canonicalize` pass (issue #34, run by `resolve_root`) recognises the
//! count-ranked shape positionally, so a SQL `ORDER BY`/`LIMIT` and a PromQL
//! `topk(...)` converge without either front end special-casing the other's
//! syntax.

use std::rc::Rc;
use std::sync::Arc;

use datafusion::arrow::datatypes::DataType as ArrowDataType;
use datafusion::catalog_common::MemorySchemaProvider;
use datafusion::common::config::ConfigOptions;
use datafusion::common::tree_node::{Transformed, TreeNode, TreeNodeRecursion};
use datafusion::common::{Column as DfColumn, DFSchema, ScalarValue as DfScalarValue};
use datafusion::datasource::MemTable;
use datafusion::functions_aggregate::count::count_udaf;
use datafusion::functions_aggregate::sum::sum_udaf;
use datafusion::logical_expr::expr::AggregateFunction;
use datafusion::logical_expr::expr_rewriter::FunctionRewrite;
use datafusion::logical_expr::{
    self, lit, AggregateUDF, Case, Distinct, Expr, JoinType, LogicalPlan, Signature,
    SimpleAggregateUDF, TypeSignature, Volatility, WindowFunctionDefinition,
};
use datafusion::optimizer::analyzer::function_rewrite::ApplyFunctionRewrites;
use datafusion::optimizer::{AnalyzerRule, OptimizerConfig};
use datafusion::prelude::{SessionConfig, SessionContext};

use asap_sql_function_catalog::{AggSemantic, Arity, RewriteKind};
use asap_types::pre_asap::agg_intent::AggIntent;
use asap_types::pre_asap::query_expr::{
    GroupKeys, Predicate, ProjectItem, Reduction, SortKey, Source,
    UnresolvedQueryExpr as Unresolved,
};
use asap_types::pre_asap::schema::{DataType, Schema};
use asap_types::pre_asap::{
    ColumnRef, CompareOp, JoinKind, ScalarValue, SetOpKind, WindowFuncKind,
};
use asap_types::types::AccuracyTarget;
use asap_types::workload::SqlDialect;

use crate::error::SqlError as LoweringError;

mod expr;
mod types;

pub use types::SqlCatalog;

use self::expr::df_expr_to_unresolved;
use self::types::{arrow_to_dtype, schema_to_arrow};

std::thread_local! {
    static ACCURACY: std::cell::RefCell<AccuracyTarget> =
        const { std::cell::RefCell::new(AccuracyTarget::Exact) };
}

/// RAII guard installing `accuracy` as the ambient accuracy target for the
/// current thread's lowering, restoring the prior value on drop — same
/// ambient-thread-local shape as `asap_frontend_promql::promql`'s
/// `AccuracyGuard`, for the same reason: it injects `accuracy` into the deep
/// `lower_plan` recursion without a parameter on every one of its
/// signatures, consulted only at the couple of sites that build an
/// accuracy-bearing `AggIntent`.
struct AccuracyGuard(AccuracyTarget);

impl AccuracyGuard {
    fn install(accuracy: AccuracyTarget) -> Self {
        let prev = ACCURACY.with(|a| a.replace(accuracy));
        AccuracyGuard(prev)
    }
}

impl Drop for AccuracyGuard {
    fn drop(&mut self) {
        ACCURACY.with(|a| *a.borrow_mut() = std::mem::replace(&mut self.0, AccuracyTarget::Exact));
    }
}

fn current_accuracy() -> AccuracyTarget {
    ACCURACY.with(|a| a.borrow().clone())
}

/// Lowers SQL strings to the canonical [`UnresolvedQueryExpr`](asap_types::pre_asap::UnresolvedQueryExpr)
/// over a table [`SqlCatalog`]. Call
/// [`resolve_root`](asap_types::pre_asap::resolve_root) on the result for
/// the canonical, resolved tree.
pub struct SqlLowerer<'a> {
    catalog: &'a SqlCatalog,
    dialect: SqlDialect,
}

impl<'a> SqlLowerer<'a> {
    pub fn new(catalog: &'a SqlCatalog) -> Self {
        Self {
            catalog,
            dialect: SqlDialect::DataFusionSQL,
        }
    }

    /// Parse under a specific SQL dialect (e.g. `ClickhouseSQL`, which maps to
    /// sqlparser's vendored `ClickHouseDialect` — array-lambda syntax and
    /// `arr[-1]` indexing parse under it that don't parse generically). This
    /// only changes *parsing*: a ClickHouse-only builtin function not listed
    /// in `asap_sql_function_catalog::CLICKHOUSE_BUILTINS` (`uniqExact` and
    /// `countIf` are; most of ClickHouse's builtin surface isn't yet) is
    /// still unknown to DataFusion's planner and still fails there, and
    /// `ElasticSQL` has no vendored parser at all.
    pub fn with_dialect(catalog: &'a SqlCatalog, dialect: SqlDialect) -> Self {
        Self { catalog, dialect }
    }

    /// Parse + lower a SQL query to the canonical, unresolved shape, threading
    /// `accuracy` onto every approximate intent (`Count`, `Quantile`,
    /// `Cardinality`) as it is built.
    ///
    /// The `AccuracyGuard` installs *after* the only `.await` point
    /// (`ctx.sql`) — `lower_plan` itself is synchronous, so once it starts
    /// there is no further suspension point that could move this task to a
    /// different OS thread out from under a thread-local set beforehand.
    ///
    /// Runs `ApplyFunctionRewrites` — the single `AnalyzerRule` DataFusion's
    /// own `Analyzer` uses internally to apply `FunctionRewrite`s, called
    /// directly rather than through `Analyzer::execute_and_check` — over the
    /// raw parsed plan before lowering, carrying only
    /// `ClickHouseBuiltinRewrite` (catalog-driven, see its own doc — it
    /// covers every `asap_sql_function_catalog::CLICKHOUSE_BUILTINS` entry,
    /// not just one). `ctx.sql(...).into_unoptimized_plan()` alone returns
    /// `SqlToRel`'s output untouched, and a `FunctionRewrite` only ever runs
    /// as part of this rule, so calling it directly is unavoidable to make
    /// the rewrite fire. Its `analyze()` already does a full
    /// `transform_up_with_subqueries` over the whole plan, so it needs no
    /// wrapping `Analyzer` at all — deliberately not
    /// `Analyzer::execute_and_check` (whether with the default 5-rule
    /// analyzer or an empty one carrying just this rewrite): that method
    /// runs an unconditional post-check (`check_plan`, hardcoded, not itself
    /// a rule) that isn't wanted here — e.g. it independently rejects a
    /// multi-column `IN (subquery)` before `lower_in_subquery`'s own arity
    /// check would. Going straight to `ApplyFunctionRewrites` avoids that
    /// entirely: zero behavior change for every query that doesn't call a
    /// catalog-listed ClickHouse builtin.
    pub async fn lower(
        &self,
        sql: &str,
        accuracy: &AccuracyTarget,
    ) -> Result<Unresolved, LoweringError> {
        let ctx = self.build_context()?;
        let df = ctx.sql(sql).await?;
        let plan = df.into_unoptimized_plan();
        let rewriter = ApplyFunctionRewrites::new(vec![Arc::new(ClickHouseBuiltinRewrite)]);
        let plan = rewriter.analyze(plan, ctx.state().options())?;
        let _guard = AccuracyGuard::install(accuracy.clone());
        self.lower_plan(&plan)
    }

    /// Register the catalog tables (empty Arrow `MemTable`s) so DataFusion can
    /// resolve table/column references during planning.
    fn build_context(&self) -> Result<SessionContext, LoweringError> {
        let dialect_name = match &self.dialect {
            SqlDialect::DataFusionSQL => "generic",
            SqlDialect::ClickhouseSQL => "ClickHouse",
            SqlDialect::ElasticSQL => {
                return Err(LoweringError::UnsupportedDialect("ElasticSQL".into()))
            }
        };
        let config = SessionConfig::new().set_str("datafusion.sql_parser.dialect", dialect_name);
        let ctx = SessionContext::new_with_config(config);
        // A catalog key like "bgp.bgp_updates" schema-qualifies the table
        // (e.g. a ClickHouse database name). DataFusion requires the parent
        // schema to be registered before a qualified table can be, so create
        // it on demand.
        let catalog_provider = ctx.catalog("datafusion").ok_or_else(|| {
            LoweringError::InvalidExpression("default \"datafusion\" catalog missing".into())
        })?;
        for (name, schema) in &self.catalog.tables {
            if let Some((schema_name, _)) = name.split_once('.') {
                if catalog_provider.schema(schema_name).is_none() {
                    catalog_provider
                        .register_schema(schema_name, Arc::new(MemorySchemaProvider::new()))?;
                }
            }
            let arrow_schema = Arc::new(schema_to_arrow(schema));
            let mem_table = MemTable::try_new(arrow_schema, vec![])?;
            ctx.register_table(name.as_str(), Arc::new(mem_table))?;
        }
        // Register a stub `AggregateUDF` for every catalog-listed
        // ClickHouse-only builtin, purely so DataFusion's planner can
        // resolve its name during parsing — `lower()` rewrites every call
        // site to a native DataFusion aggregate via `ClickHouseBuiltinRewrite`
        // before `lower_plan` sees it.
        for builtin in asap_sql_function_catalog::CLICKHOUSE_BUILTINS {
            ctx.register_udaf(clickhouse_builtin_stub_udaf(builtin.name, builtin.arity));
        }
        Ok(ctx)
    }

    fn lower_plan(&self, plan: &LogicalPlan) -> Result<Unresolved, LoweringError> {
        match plan {
            LogicalPlan::TableScan(scan) => self.lower_table_scan(scan),
            LogicalPlan::Filter(filter) => self.lower_filter(filter),
            LogicalPlan::Projection(proj) => self.lower_projection(proj),
            LogicalPlan::Aggregate(agg) => self.lower_aggregate(agg),
            LogicalPlan::Sort(sort) => self.lower_sort(sort),
            LogicalPlan::Limit(limit) => self.lower_limit(limit),
            LogicalPlan::Distinct(d) => match d {
                Distinct::On(_) => Err(LoweringError::UnsupportedFeature("DISTINCT ON".into())),
                Distinct::All(input) => Ok(Unresolved::Distinct {
                    cols: vec![],
                    child: Rc::new(self.lower_plan(input)?),
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
                    Ok(Unresolved::SetOp {
                        kind: SetOpKind::Union,
                        all: true,
                        left: Rc::new(left),
                        right: Rc::new(self.lower_plan(right_plan)?),
                    })
                })
            }
            LogicalPlan::Window(window) => self.lower_window(window),
            LogicalPlan::Join(join) => self.lower_join(join),
            LogicalPlan::Subquery(_) => Err(LoweringError::UnsupportedFeature("subquery".into())),
            LogicalPlan::SubqueryAlias(alias) => {
                // An alias over a table re-qualifies the scan's columns with the
                // alias (so `a.col` / `b.col` in a self-join disambiguate).
                match alias.input.as_ref() {
                    LogicalPlan::TableScan(scan) => {
                        self.scan_source(&scan.table_name.to_string(), &alias.alias.to_string())
                    }
                    // A *derived table* / inline view — `FROM (SELECT …) t`, the
                    // SQL counterpart of PromQL function nesting (an aggregate
                    // over an aggregate, a filter over a derived aggregate, …).
                    // Lower the inner plan, then re-qualify its output columns
                    // with the alias so `t.col` resolves to *this* relation — and,
                    // critically, so a join over two derived tables disambiguates
                    // its keys instead of both binding to the first bare-name
                    // match (issue #66). The inner column *names* are unchanged;
                    // only the qualifier is stamped.
                    other => {
                        let alias_name = alias.alias.to_string();
                        match self.lower_plan(other)? {
                            // The derived SELECT list already lowered to a
                            // Projection — stamp the alias onto it, no extra node.
                            Unresolved::Project { cols, child, .. } => Ok(Unresolved::Project {
                                cols,
                                qualifier: Some(alias_name),
                                child,
                            }),
                            // Otherwise (e.g. `SELECT *` unwrapped to a scan) wrap
                            // in an identity projection that re-qualifies each
                            // output column. Names come from the sub-plan's schema.
                            inner => {
                                let cols = alias
                                    .input
                                    .schema()
                                    .fields()
                                    .iter()
                                    .map(|f| ProjectItem {
                                        alias: Some(f.name().clone()),
                                        expr: Unresolved::Column(ColumnRef::Named(
                                            f.name().clone(),
                                        )),
                                    })
                                    .collect();
                                Ok(Unresolved::Project {
                                    cols,
                                    qualifier: Some(alias_name),
                                    child: Rc::new(inner),
                                })
                            }
                        }
                    }
                }
            }
            other => Err(LoweringError::UnsupportedFeature(format!(
                "plan node: {}",
                other.display()
            ))),
        }
    }

    /// `WHERE` — a conjunction of ordinary predicates plus, possibly, subquery
    /// predicates (issue #111).
    ///
    /// `c IN (SELECT …)` and `EXISTS (…)` are not expressions over rows; they are
    /// *joins*. Each such conjunct peels off into a semi- / anti-join above the
    /// filter's input, and the remaining conjuncts stay as an ordinary `Filter`.
    ///
    /// The residual filter is applied **below** the joins, which is where it sat
    /// before: a semi-join only ever drops left rows, so the two orders agree —
    /// and keeping the fold-onto-`Scan` (`filter_or_fold`) below the joins
    /// matches where the old converter folded it too.
    fn lower_filter(&self, filter: &logical_expr::Filter) -> Result<Unresolved, LoweringError> {
        let mut conjuncts = Vec::new();
        split_conjunction(&filter.predicate, &mut conjuncts);
        let (subqueries, residual): (Vec<_>, Vec<_>) = conjuncts
            .into_iter()
            .partition(|e| matches!(e, Expr::InSubquery(_) | Expr::Exists(_)));

        let input = self.lower_plan(&filter.input)?;
        let mut node = match rebuild_conjunction(&residual) {
            Some(pred) => filter_or_fold(df_expr_to_unresolved(&pred)?, input),
            None => input,
        };
        for sq in subqueries {
            node = match sq {
                Expr::InSubquery(is) => self.lower_in_subquery(is, node)?,
                Expr::Exists(ex) => self.lower_exists(ex, node)?,
                _ => unreachable!("partitioned above"),
            };
        }
        Ok(node)
    }

    /// `c IN (SELECT k FROM …)` → a semi-join on `c = k` (issue #111).
    fn lower_in_subquery(
        &self,
        is: &logical_expr::expr::InSubquery,
        left: Unresolved,
    ) -> Result<Unresolved, LoweringError> {
        if is.negated {
            // `NOT IN` is not an anti-join. Under three-valued logic a single
            // NULL among the subquery's rows makes `c NOT IN (…)` UNKNOWN for
            // every `c`, so the query returns nothing — while an anti-join
            // returns every unmatched left row. Reject rather than mislower.
            return Err(LoweringError::UnsupportedFeature(
                "NOT IN (subquery): its NULL semantics are not an anti-join".into(),
            ));
        }
        if !is.subquery.outer_ref_columns.is_empty() {
            return Err(LoweringError::UnsupportedFeature(
                "correlated IN (subquery)".into(),
            ));
        }
        let inner = is.subquery.subquery.as_ref();
        let fields = inner.schema().fields();
        if fields.len() != 1 {
            return Err(LoweringError::InvalidExpression(format!(
                "IN (subquery) must select exactly one column, got {}",
                fields.len()
            )));
        }
        let key = &fields[0];
        // Project the key under a name the outer relation cannot also carry. The
        // join predicate resolves against the concatenated `left ++ right`
        // schema, and a bare `hosts.service` over an unqualified subquery output
        // falls back to a name lookup that finds the *left's* `service` first —
        // silently making the predicate `service = service`, i.e. always true.
        let right = match inner {
            // Rebuild the subquery's projection with the synthetic alias, so a
            // computed key (`SELECT bytes + 1 …`) is named rather than becoming
            // the anonymous `col_0` that nothing can reference.
            LogicalPlan::Projection(p) if p.expr.len() == 1 => Unresolved::Project {
                cols: vec![ProjectItem {
                    alias: Some(IN_SUBQUERY_KEY.to_string()),
                    expr: df_expr_to_unresolved(unalias(&p.expr[0]))?,
                }],
                qualifier: None,
                child: Rc::new(self.lower_plan(&p.input)?),
            },
            other => Unresolved::Project {
                cols: vec![ProjectItem {
                    alias: Some(IN_SUBQUERY_KEY.to_string()),
                    expr: Unresolved::Column(ColumnRef::Named(key.name().clone())),
                }],
                qualifier: None,
                child: Rc::new(self.lower_plan(other)?),
            },
        };
        Ok(Unresolved::Join {
            kind: JoinKind::Semi,
            pred: Predicate(Rc::new(Unresolved::Compare {
                left: Rc::new(df_expr_to_unresolved(&is.expr)?),
                op: CompareOp::Eq,
                right: Rc::new(Unresolved::Column(ColumnRef::Named(
                    IN_SUBQUERY_KEY.to_string(),
                ))),
            })),
            left: Rc::new(left),
            right: Rc::new(right),
        })
    }

    /// `[NOT] EXISTS (SELECT … WHERE inner.k = outer.k)` → a semi- / anti-join
    /// on the correlation predicate (issue #111).
    fn lower_exists(
        &self,
        ex: &logical_expr::expr::Exists,
        left: Unresolved,
    ) -> Result<Unresolved, LoweringError> {
        let kind = if ex.negated {
            JoinKind::Anti
        } else {
            JoinKind::Semi
        };
        // A semi-join discards the right side's columns, and `SELECT 1` projects
        // the correlation columns away — so drop the subquery's projections and
        // join against what they sit on.
        let mut inner = ex.subquery.subquery.as_ref();
        while let LogicalPlan::Projection(p) = inner {
            inner = &p.input;
        }
        // Lift the correlated conjuncts out of the subquery's filter; they are
        // the join predicate. Whatever is left stays an ordinary inner filter.
        let (inner, correlation) = split_correlation(inner)?;
        let right = self.lower_plan(&inner)?;
        // No correlation conjunct (a genuinely uncorrelated `EXISTS`) means
        // the join condition is unconditionally true — same convention as an
        // unconditional `JOIN` (`lower_join`, below).
        let pred = match correlation {
            Some(e) => Predicate(Rc::new(df_expr_to_unresolved(&e)?)),
            None => Predicate(Rc::new(Unresolved::Literal(ScalarValue::Boolean(true)))),
        };
        Ok(Unresolved::Join {
            kind,
            pred,
            left: Rc::new(left),
            right: Rc::new(right),
        })
    }

    /// Table leaf — carries the catalog's resolved schema directly on `Scan`
    /// (`schema: Some(_)`), so `resolve_root`'s Binder doesn't need to
    /// usage-derive it (SQL is never schemaless). Projection pushdown is left
    /// to the enclosing `Project` (DataFusion's unoptimized plan sets no
    /// projection).
    fn lower_table_scan(
        &self,
        scan: &logical_expr::TableScan,
    ) -> Result<Unresolved, LoweringError> {
        let table = scan.table_name.to_string();
        self.scan_source(&table, &table)
    }

    /// A `Scan` over catalog table `table`, with its columns qualified by
    /// `qualifier` (the table name, or an alias from a `SubqueryAlias`) so
    /// `Qualified` column refs resolve to the right side across a join.
    fn scan_source(&self, table: &str, qualifier: &str) -> Result<Unresolved, LoweringError> {
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
        Ok(Unresolved::Scan {
            source: Source::Table {
                table_ref: table.to_string(),
            },
            predicates: vec![],
            schema: Some(qualified),
        })
    }

    /// ⋈ — equijoin. The `on` key pairs become `left = right` comparisons,
    /// AND-ed with any non-equi `filter`, into the join predicate — still
    /// name-based here (like a `WHERE`); `resolve_root` derives the
    /// concatenated output schema downstream. Semi/anti/mark joins have no
    /// canonical counterpart yet and are rejected.
    fn lower_join(&self, join: &logical_expr::Join) -> Result<Unresolved, LoweringError> {
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
                Ok(Unresolved::Compare {
                    left: Rc::new(df_expr_to_unresolved(l)?),
                    op: CompareOp::Eq,
                    right: Rc::new(df_expr_to_unresolved(r)?),
                })
            })
            .collect::<Result<Vec<_>, LoweringError>>()?;
        if let Some(filter) = &join.filter {
            conjuncts.push(df_expr_to_unresolved(filter)?);
        }
        let pred = Predicate(Rc::new(match conjuncts.len() {
            // No condition (a CROSS JOIN) is unconditionally true.
            0 => Unresolved::Literal(ScalarValue::Boolean(true)),
            1 => conjuncts.pop().unwrap(),
            _ => Unresolved::BoolAnd(conjuncts),
        }));
        Ok(Unresolved::Join {
            kind,
            pred,
            left: Rc::new(self.lower_plan(&join.left)?),
            right: Rc::new(self.lower_plan(&join.right)?),
        })
    }

    /// `func(args) OVER (PARTITION BY … ORDER BY …)`. One window function per
    /// plan node; window frames are not modelled yet (default frame assumed).
    fn lower_window(&self, window: &logical_expr::Window) -> Result<Unresolved, LoweringError> {
        if window.window_expr.len() > 1 {
            return Err(LoweringError::UnsupportedFeature(format!(
                "multiple window functions in one plan node (got {}); split them",
                window.window_expr.len()
            )));
        }
        let child = Rc::new(self.lower_plan(&window.input)?);
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
            .map(df_expr_to_unresolved)
            .collect::<Result<Vec<_>, _>>()?;
        // Nth_value: lift N from the (literal) 2nd arg, keep only the column.
        let func = if matches!(func, WindowFuncKind::NthValue(None)) {
            let n = match args.get(1) {
                Some(Unresolved::Literal(ScalarValue::Int64(n))) if *n > 0 => *n as u64,
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
                df_expr_to_unresolved(&s.expr).map(|expr| SortKey {
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
        Ok(Unresolved::WindowFunc {
            func,
            args,
            partition_by: partition_by.into(),
            order_by,
            output_name,
            child,
        })
    }

    fn lower_projection(
        &self,
        proj: &logical_expr::Projection,
    ) -> Result<Unresolved, LoweringError> {
        // SELECT * — no column constraint; pass through without a Project.
        if proj.expr.iter().any(|e| matches!(e, Expr::Wildcard { .. })) {
            return self.lower_plan(&proj.input);
        }
        let child = Rc::new(self.lower_plan(&proj.input)?);
        let cols = proj
            .expr
            .iter()
            .map(|e| match e {
                Expr::Alias(a) => df_expr_to_unresolved(&a.expr).map(|expr| ProjectItem {
                    expr,
                    alias: Some(a.name.clone()),
                }),
                _ => df_expr_to_unresolved(e).map(|expr| ProjectItem { expr, alias: None }),
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Unresolved::Project {
            cols,
            qualifier: None,
            child,
        })
    }

    fn lower_aggregate(&self, agg: &logical_expr::Aggregate) -> Result<Unresolved, LoweringError> {
        let input = self.lower_plan(&agg.input)?;

        // `GROUPING SETS`/`ROLLUP`/`CUBE` emit several grouping levels from one
        // scan. `Aggregate.by` is a single key set, so each level becomes its own
        // `Aggregate` and they are merged (issue #118).
        if let Some(gs) = agg.group_expr.iter().find_map(as_grouping_set) {
            return self.lower_grouping_sets(agg, gs, input);
        }

        // `Aggregate.by` and the reducers index *columns*, so a grouping or
        // reducer expression (`GROUP BY date_trunc(…)`, `SUM(a * 8)`) has no
        // slot. Materialize each one as a derived column in a `Project` beneath
        // the aggregate, then group/reduce over that column (issue #110).
        let mut derived = DerivedCols::default();

        // DataFusion strips `AS m` from a grouping expression, so the aggregate
        // schema's field name is what the enclosing Projection references —
        // the derived column has to carry exactly that name.
        let group_names: Vec<String> = agg
            .schema
            .fields()
            .iter()
            .take(agg.group_expr.len())
            .map(|f| f.name().to_string())
            .collect();

        let mut keys = Vec::with_capacity(agg.group_expr.len());
        for (i, e) in agg.group_expr.iter().enumerate() {
            match unalias(e) {
                Expr::Column(_) => {
                    derived.passthrough(e)?;
                    keys.push(expr_to_group_ref(e)?);
                }
                other => {
                    let name = group_names
                        .get(i)
                        .cloned()
                        .unwrap_or_else(|| other.to_string());
                    derived.materialize(name.clone(), df_expr_to_unresolved(other)?)?;
                    keys.push(ColumnRef::Named(name));
                }
            }
        }

        // Reducer arguments get the same treatment; `rewrite_agg` returns the
        // aggregate with its argument repointed at the derived column.
        let aggr_expr = agg
            .aggr_expr
            .iter()
            .map(|e| derived.rewrite_agg(e))
            .collect::<Result<Vec<_>, LoweringError>>()?;

        let child = Rc::new(derived.wrap(input)?);
        // DataFusion names the aggregate outputs in its own schema (e.g.
        // "sum(metrics.bytes)") — the same names the enclosing Projection
        // references. The schema is [group fields …, aggregate fields …], so
        // skip the group fields and thread the rest straight through as
        // `Aggregate.output_names`, letting that Projection resolve them.
        let output_names: Vec<String> = agg
            .schema
            .fields()
            .iter()
            .skip(agg.group_expr.len())
            .map(|f| f.name().to_string())
            .collect();
        let measures = aggr_expr
            .iter()
            .map(lower_agg_intent)
            .collect::<Result<Vec<_>, LoweringError>>()?;
        Ok(Unresolved::Aggregate {
            // SQL `GROUP BY` is always an inclusion list, never PromQL's
            // `without(...)` exclusion form — and always a genuine reduction,
            // never `PerEntity` (there's no windowed/subquery-child concept
            // in SQL for that to apply to).
            reduction: Reduction::Reduce(GroupKeys::by(keys)),
            measures,
            output_names,
            having: None,
            child,
        })
    }

    /// `GROUP BY ROLLUP/CUBE/GROUPING SETS` — multi-level grouping (issue #118).
    ///
    /// One scan produces several grouping levels; `Aggregate.by` holds a single
    /// key set. So each level becomes its own `Aggregate`, and the levels are
    /// `Merge`d. A level that omits a key still has to *emit* it — as `NULL`, per
    /// SQL — so each branch is wrapped in a `Project` that reinstates the missing
    /// keys as typed nulls and restores the canonical column order. That keeps
    /// the branches union-compatible, which `Merge` requires (it derives its
    /// schema from the first child).
    ///
    /// `Aggregate.child` is duplicated per level — the same trade
    /// `histogram_quantiles` makes (#109); a future workload-level reuse pass
    /// could hoist it back into a single producer.
    ///
    /// DataFusion's `__grouping_id` discriminator is dropped: it only exists to
    /// tell a subtotal's `NULL` apart from a data `NULL`, which is observable
    /// solely through `GROUPING(col)` — an aggregate this front end rejects.
    fn lower_grouping_sets(
        &self,
        agg: &logical_expr::Aggregate,
        gs: &logical_expr::GroupingSet,
        input: Unresolved,
    ) -> Result<Unresolved, LoweringError> {
        // DataFusion normalizes every mixed form (`GROUP BY g, ROLLUP(d)`) into a
        // single `GroupingSets`, so one grouping expression is the only shape.
        if agg.group_expr.len() != 1 {
            return Err(LoweringError::UnsupportedFeature(
                "a grouping set alongside plain GROUP BY keys".into(),
            ));
        }

        // `distinct_expr()` is ordered exactly like the aggregate's leading
        // schema fields, which is the column order the enclosing Projection
        // expects. The field after them is `__grouping_id`.
        let distinct = gs.distinct_expr();
        for e in &distinct {
            if !matches!(unalias(e), Expr::Column(_)) {
                return Err(LoweringError::UnsupportedFeature(format!(
                    "non-column key inside a multi-level grouping: {e}"
                )));
            }
        }
        let keys: Vec<(String, DataType)> = agg
            .schema
            .fields()
            .iter()
            .take(distinct.len())
            .map(|f| Ok((f.name().to_string(), arrow_to_dtype(f.data_type())?)))
            .collect::<Result<_, LoweringError>>()?;

        let output_names: Vec<String> = agg
            .schema
            .fields()
            .iter()
            .skip(distinct.len() + 1) // + `__grouping_id`
            .map(|f| f.name().to_string())
            .collect();

        // Reducer arguments still materialize as derived columns (#110); the
        // grouping keys are plain columns, so they only need carrying through.
        let mut derived = DerivedCols::default();
        for e in &distinct {
            derived.passthrough(e)?;
        }
        let aggr_expr = agg
            .aggr_expr
            .iter()
            .map(|e| derived.rewrite_agg(e))
            .collect::<Result<Vec<_>, LoweringError>>()?;
        let measures = aggr_expr
            .iter()
            .map(lower_agg_intent)
            .collect::<Result<Vec<_>, LoweringError>>()?;
        let input = derived.wrap(input)?;

        let branches = expand_grouping_set(gs)
            .iter()
            .map(|level| {
                let level_keys = distinct
                    .iter()
                    .filter(|e| level.contains(e))
                    .map(|e| expr_to_group_ref(e))
                    .collect::<Result<Vec<_>, LoweringError>>()?;
                let aggregate = Unresolved::Aggregate {
                    reduction: Reduction::Reduce(GroupKeys::by(level_keys)),
                    measures: measures.clone(),
                    output_names: output_names.clone(),
                    having: None,
                    child: Rc::new(input.clone()),
                };
                // Reinstate omitted keys as typed nulls, in canonical order.
                let cols = keys
                    .iter()
                    .zip(&distinct)
                    .map(|((name, dtype), e)| ProjectItem {
                        alias: Some(name.clone()),
                        expr: if level.contains(e) {
                            Unresolved::Column(ColumnRef::Named(name.clone()))
                        } else {
                            Unresolved::Cast {
                                expr: Rc::new(Unresolved::Literal(ScalarValue::Null)),
                                to: dtype.clone(),
                                try_cast: false,
                            }
                        },
                    })
                    .chain(output_names.iter().map(|n| ProjectItem {
                        alias: Some(n.clone()),
                        expr: Unresolved::Column(ColumnRef::Named(n.clone())),
                    }))
                    .collect();
                Ok(Unresolved::Project {
                    cols,
                    qualifier: None,
                    child: Rc::new(aggregate),
                })
            })
            .collect::<Result<Vec<_>, LoweringError>>()?;

        Ok(Unresolved::Merge { children: branches })
    }

    fn lower_sort(&self, sort: &logical_expr::Sort) -> Result<Unresolved, LoweringError> {
        // A count-ranked `ORDER BY … LIMIT k` is the frequency heavy-hitter the
        // `TopK` intent represents, but that promotion now happens in the shared
        // `canonicalize` pass (issue #34) — the same one both front ends run —
        // so SQL emits a plain `Sort` (+ `Limit`) here and lets canonicalization
        // recognise the count-ranked shape positionally. This removes the gate's
        // alias blind spot (#20).
        let keys = sort
            .expr
            .iter()
            .map(|s| {
                df_expr_to_unresolved(&s.expr).map(|expr| SortKey {
                    expr,
                    ascending: s.asc,
                    nulls_first: s.nulls_first,
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Unresolved::Sort {
            keys,
            // SQL `ORDER BY` is a global sort; per-group ranking would come from a
            // window function (`WindowFunc`), not a bare Sort.
            partition_by: GroupKeys::none(),
            child: Rc::new(self.lower_plan(&sort.input)?),
        })
    }

    fn lower_limit(&self, limit: &logical_expr::Limit) -> Result<Unresolved, LoweringError> {
        // Count-ranked `LIMIT k` over a `Sort` is promoted to the heavy-hitter
        // `TopK` by the shared `canonicalize` pass (issue #34), not here.
        Ok(Unresolved::Limit {
            n: eval_fetch(&limit.fetch).unwrap_or(usize::MAX),
            offset: eval_fetch(&limit.skip).unwrap_or(0),
            child: Rc::new(self.lower_plan(&limit.input)?),
        })
    }
}

// ── ClickHouse-builtin compatibility, taught to DataFusion itself ──────────────
//
// Generalized over `asap_sql_function_catalog::CLICKHOUSE_BUILTINS` (issue
// #225): adding support for one more ClickHouse-only builtin DataFusion
// doesn't know at all is a catalog data entry (name, arity, `RewriteKind`)
// plus, only if its rewrite target is a genuinely new shape, one match arm
// in `ClickHouseBuiltinRewrite::rewrite` below — never a new stub-UDAF
// constructor or a new `FunctionRewrite`-implementing type. `uniqExact`
// (issue #221) and `countIf` both go through this one mechanism.

/// A stub `AggregateUDF` for one `CLICKHOUSE_BUILTINS` entry, registered
/// purely so DataFusion's planner can resolve the function name during
/// `SqlToRel` conversion (it errors on an unknown function before a rewrite
/// ever gets a chance to run). Every call site is replaced by
/// `ClickHouseBuiltinRewrite` — via the `Analyzer` `lower()` runs after
/// parsing — before physical planning could ever ask this UDAF for an
/// `Accumulator`, so `accumulator` is unreachable for every catalog entry.
fn clickhouse_builtin_stub_udaf(name: &'static str, arity: Arity) -> AggregateUDF {
    AggregateUDF::from(SimpleAggregateUDF::new_with_signature(
        name,
        arity_to_signature(arity),
        ArrowDataType::Int64,
        Arc::new(move |_| {
            // ponytail: dead code by construction (see doc comment above) —
            // a real accumulator would just reimplement whatever native
            // shape `ClickHouseBuiltinRewrite` rewrites this call to.
            unimplemented!(
                "{name} has no accumulator: every call site is rewritten to a native \
                 DataFusion aggregate before physical planning"
            )
        }),
        vec![],
    ))
}

/// A catalog [`Arity`] as the DataFusion `Signature` a stub UDAF is
/// registered with.
fn arity_to_signature(arity: Arity) -> Signature {
    match arity {
        Arity::Exact(n) => Signature::any(n, Volatility::Immutable),
        Arity::Range { min, max } => Signature::one_of(
            (min..=max).map(TypeSignature::Any).collect(),
            Volatility::Immutable,
        ),
    }
}

/// Rewrites every `asap_sql_function_catalog::CLICKHOUSE_BUILTINS` call to
/// the native DataFusion aggregate shape its entry's `RewriteKind` names —
/// so a ClickHouse-only builtin DataFusion doesn't know at all becomes an
/// ordinary DataFusion aggregate before the plan ever reaches
/// `lower_agg_intent`, which needs no ClickHouse-specific name of its own.
#[derive(Debug)]
struct ClickHouseBuiltinRewrite;

impl FunctionRewrite for ClickHouseBuiltinRewrite {
    fn name(&self) -> &str {
        "clickhouse builtin -> native DataFusion aggregate"
    }

    fn rewrite(
        &self,
        expr: Expr,
        _schema: &DFSchema,
        _config: &ConfigOptions,
    ) -> datafusion::common::Result<Transformed<Expr>> {
        let Expr::AggregateFunction(f) = expr else {
            return Ok(Transformed::no(expr));
        };
        let Some(builtin) = asap_sql_function_catalog::lookup_clickhouse_builtin(f.func.name())
        else {
            return Ok(Transformed::no(Expr::AggregateFunction(f)));
        };
        let rewritten = match builtin.rewrite {
            // `f(args...)` -> `count(args...) DISTINCT` — `lower_agg_intent`
            // already maps `count` + `DISTINCT` to `AggIntent::Cardinality`.
            RewriteKind::CountDistinct => AggregateFunction::new_udf(
                count_udaf(),
                f.args,
                true,
                f.filter,
                f.order_by,
                f.null_treatment,
            ),
            // `f(cond)` -> `sum(CASE WHEN cond THEN 1 ELSE 0 END)` — see
            // `RewriteKind::CountIfToSum`'s doc for why a plain `count(...)
            // FILTER (WHERE cond)` doesn't work here (`AggIntent::Count`
            // never consults its argument).
            RewriteKind::CountIfToSum => {
                let cond = f.args.into_iter().next().expect(
                    "countif's stub signature fixes its arity at 1 -- the planner \
                     already rejected any other argument count before this rewrite runs",
                );
                let indicator = Expr::Case(Case::new(
                    None,
                    vec![(Box::new(cond), Box::new(lit(1i64)))],
                    Some(Box::new(lit(0i64))),
                ));
                AggregateFunction::new_udf(
                    sum_udaf(),
                    vec![indicator],
                    false,
                    f.filter,
                    f.order_by,
                    f.null_treatment,
                )
            }
        };
        Ok(Transformed::yes(Expr::AggregateFunction(rewritten)))
    }
}

// ── Aggregate / group-key helpers ───────────────────────────────────────────────

/// Map a DataFusion aggregate expression directly to the canonical
/// [`AggIntent<ColumnRef>`] — issue #179's "dedicated function → canonical
/// intent directly" front-end construction, no `AggFunc` intermediate. The
/// name → semantic mapping itself lives in `asap_sql_function_catalog`
/// (issue #225) as flat data (`NATIVE_FUNCTIONS`); what stays here is
/// call-site logic that isn't a function of the name alone — the DISTINCT
/// modifier rule, the "reducer argument must be a bare column" rule
/// (`reducer_col`), φ extraction from a literal argument, and the ambient
/// `AccuracyTarget`. `resolve_root` resolves `col` to a positional
/// `ColumnId`; the output name (DataFusion's own, e.g.
/// `"sum(metrics.bytes)"`) is threaded separately as `Aggregate.output_names`,
/// not carried here.
fn lower_agg_intent(expr: &Expr) -> Result<AggIntent<ColumnRef>, LoweringError> {
    match expr {
        Expr::Alias(a) => lower_agg_intent(&a.expr),
        Expr::AggregateFunction(agg_fn) => {
            let name = agg_fn.func.name().to_lowercase();
            let semantic = asap_sql_function_catalog::lookup_native(&name)
                .ok_or_else(|| LoweringError::UnsupportedAggregate(name.clone()))?;
            // The canonical intent algebra has no DISTINCT modifier for the
            // value reducers; only
            // COUNT(DISTINCT) maps (to Cardinality). Reject DISTINCT elsewhere
            // rather than silently lowering `SUM(DISTINCT x)` as `SUM(x)`.
            if agg_fn.distinct && !matches!(semantic, AggSemantic::Count) {
                return Err(LoweringError::UnsupportedAggregate(format!(
                    "DISTINCT {name}"
                )));
            }
            // Value reducers (`reducer_col`) require a real column — `SUM(a*b)`
            // is rejected, not silently reduced over a probe column. Quantile
            // and CountDistinct reduce a column too, so they take the same path:
            // `col` is `Option<ColumnId>` once resolved, where `None` means "the
            // PromQL sample value", which a SQL query never has. Taking an
            // expression here would set `col: None` and silently drop it (#115).
            let col = |args: &[Expr]| -> Result<Option<ColumnRef>, LoweringError> {
                reducer_col(&name, args).map(Some)
            };
            Ok(match semantic {
                AggSemantic::Count if agg_fn.distinct => AggIntent::Cardinality {
                    col: col(&agg_fn.args)?,
                    accuracy: current_accuracy(),
                },
                AggSemantic::Count => AggIntent::Count {
                    accuracy: current_accuracy(),
                },
                AggSemantic::Sum => AggIntent::Sum {
                    col: col(&agg_fn.args)?,
                },
                AggSemantic::Min => AggIntent::Min {
                    col: col(&agg_fn.args)?,
                },
                AggSemantic::Max => AggIntent::Max {
                    col: col(&agg_fn.args)?,
                },
                AggSemantic::Avg => AggIntent::Avg {
                    col: col(&agg_fn.args)?,
                },
                AggSemantic::StdDev { population } => AggIntent::StdDev {
                    col: col(&agg_fn.args)?,
                    population,
                },
                AggSemantic::Variance { population } => AggIntent::Variance {
                    col: col(&agg_fn.args)?,
                    population,
                },
                // `fixed_q = Some(0.5)` is `median`/`approx_median`. As with
                // `approx_distinct` and `approx_percentile_cont`, the
                // `approx_` prefix does not force an approximation: the
                // sketch-vs-exact choice is the AccuracyTarget's (see
                // `plan::boundary`), so both spellings share one intent
                // (#111).
                AggSemantic::Quantile { fixed_q } => AggIntent::Quantile {
                    col: col(&agg_fn.args)?,
                    q: match fixed_q {
                        Some(q) => q,
                        None => extract_percentile_q(&agg_fn.args)?,
                    },
                    accuracy: current_accuracy(),
                },
                AggSemantic::Cardinality => AggIntent::Cardinality {
                    col: col(&agg_fn.args)?,
                    accuracy: current_accuracy(),
                },
            })
        }
        _ => Err(LoweringError::UnsupportedAggregate(format!("{expr:?}"))),
    }
}

/// The name an `IN (subquery)`'s key column is projected under, so the join
/// predicate cannot bind it to a same-named column of the outer relation.
const IN_SUBQUERY_KEY: &str = "__asap_in_key";

/// Fold `pred` directly onto `child.predicates` when `child` is a bare `Scan`
/// (a `WHERE` directly over a table), otherwise wrap it in an ordinary
/// `Filter` — canonical's invariant that a `Filter` never sits directly over a
/// `Scan`. A front end emitting the canonical shape directly is responsible
/// for maintaining that invariant itself (issue #179).
fn filter_or_fold(pred: Unresolved, child: Unresolved) -> Unresolved {
    match child {
        Unresolved::Scan {
            source,
            mut predicates,
            schema,
        } => {
            predicates.push(Predicate(Rc::new(pred)));
            Unresolved::Scan {
                source,
                predicates,
                schema,
            }
        }
        other => Unresolved::Filter {
            pred: Predicate(Rc::new(pred)),
            child: Rc::new(other),
        },
    }
}

/// Flatten a top-level `AND` chain into its conjuncts.
fn split_conjunction<'a>(expr: &'a Expr, out: &mut Vec<&'a Expr>) {
    match expr {
        Expr::BinaryExpr(b) if b.op == logical_expr::Operator::And => {
            split_conjunction(&b.left, out);
            split_conjunction(&b.right, out);
        }
        other => out.push(other),
    }
}

/// Re-`AND` the conjuncts, or `None` when there are none left.
fn rebuild_conjunction(conjuncts: &[&Expr]) -> Option<Expr> {
    conjuncts
        .iter()
        .map(|e| (*e).clone())
        .reduce(|acc, e| acc.and(e))
}

/// Split a correlated subquery's plan into `(uncorrelated plan, correlation)`.
///
/// The correlation is the conjunction of the filter conjuncts that mention an
/// outer column, rewritten so `outer_ref(t.c)` becomes a plain `t.c` — it then
/// resolves against the join's concatenated `left ++ right` schema, like any
/// other join predicate. Everything else stays an ordinary inner `Filter`.
///
/// An outer reference anywhere but a top-level filter conjunct is rejected: it
/// would need real decorrelation, not a predicate lift.
fn split_correlation(plan: &LogicalPlan) -> Result<(LogicalPlan, Option<Expr>), LoweringError> {
    let LogicalPlan::Filter(filter) = plan else {
        return if plan_has_outer_ref(plan) {
            Err(LoweringError::UnsupportedFeature(
                "correlated subquery whose outer reference is not a filter conjunct".into(),
            ))
        } else {
            Ok((plan.clone(), None))
        };
    };

    let mut conjuncts = Vec::new();
    split_conjunction(&filter.predicate, &mut conjuncts);
    let (correlated, inner): (Vec<_>, Vec<_>) =
        conjuncts.into_iter().partition(|e| expr_has_outer_ref(e));

    let input = filter.input.as_ref();
    if plan_has_outer_ref(input) {
        return Err(LoweringError::UnsupportedFeature(
            "correlated subquery whose outer reference is below its filter".into(),
        ));
    }

    let correlation = rebuild_conjunction(&correlated)
        .map(|e| strip_outer_refs(&e))
        .transpose()?;
    let plan = match rebuild_conjunction(&inner) {
        Some(pred) => LogicalPlan::Filter(
            logical_expr::Filter::try_new(pred, filter.input.clone())
                .map_err(LoweringError::DataFusion)?,
        ),
        None => input.clone(),
    };
    Ok((plan, correlation))
}

/// Rewrite `outer_ref(t.c)` to `t.c` so the expression resolves against the
/// join's concatenated schema.
fn strip_outer_refs(expr: &Expr) -> Result<Expr, LoweringError> {
    expr.clone()
        .transform(|e| {
            Ok(match e {
                Expr::OuterReferenceColumn(_, col) => Transformed::yes(Expr::Column(col)),
                other => Transformed::no(other),
            })
        })
        .map(|t| t.data)
        .map_err(LoweringError::DataFusion)
}

fn expr_has_outer_ref(expr: &Expr) -> bool {
    let mut found = false;
    expr.apply(|e| {
        if matches!(e, Expr::OuterReferenceColumn(..)) {
            found = true;
            return Ok(TreeNodeRecursion::Stop);
        }
        Ok(TreeNodeRecursion::Continue)
    })
    .expect("infallible visitor");
    found
}

fn plan_has_outer_ref(plan: &LogicalPlan) -> bool {
    let mut found = false;
    plan.apply(|p| {
        if p.expressions().iter().any(expr_has_outer_ref) {
            found = true;
            return Ok(TreeNodeRecursion::Stop);
        }
        Ok(TreeNodeRecursion::Continue)
    })
    .expect("infallible visitor");
    found
}

/// Strip `AS alias` wrappers.
fn unalias(expr: &Expr) -> &Expr {
    match expr {
        Expr::Alias(a) => unalias(&a.expr),
        other => other,
    }
}

/// The `GroupingSet` inside a grouping expression, if any.
fn as_grouping_set(expr: &Expr) -> Option<&logical_expr::GroupingSet> {
    match unalias(expr) {
        Expr::GroupingSet(gs) => Some(gs),
        _ => None,
    }
}

/// The grouping levels a `GroupingSet` stands for, widest first (issue #118).
///
/// `ROLLUP(a, b)` → `(a,b), (a), ()` — the prefixes.
/// `CUBE(a, b)`   → `(a,b), (a), (b), ()` — the power set.
/// `GROUPING SETS` is already the explicit list.
fn expand_grouping_set(gs: &logical_expr::GroupingSet) -> Vec<Vec<Expr>> {
    match gs {
        logical_expr::GroupingSet::Rollup(exprs) => (0..=exprs.len())
            .rev()
            .map(|n| exprs[..n].to_vec())
            .collect(),
        logical_expr::GroupingSet::Cube(exprs) => {
            // Bitmask descending, so the full set leads and `()` trails.
            (0..(1u32 << exprs.len()))
                .rev()
                .map(|mask| {
                    exprs
                        .iter()
                        .enumerate()
                        .filter(|(i, _)| mask & (1 << i) != 0)
                        .map(|(_, e)| e.clone())
                        .collect()
                })
                .collect()
        }
        logical_expr::GroupingSet::GroupingSets(sets) => sets.clone(),
    }
}

/// Derived columns materialized in a `Project` beneath an `Aggregate` (#110).
///
/// `Aggregate.by` holds positional `ColumnId`s and each reducer holds one input
/// column, so neither can hold an expression. `GROUP BY date_trunc('minute', t)`
/// and `SUM(bytes * 8)` are therefore rewritten to group/reduce over a projected
/// column that carries the expression's value.
///
/// The projection also has to carry through the plain columns the aggregate
/// still references, since a `Project` replaces its child's schema rather than
/// extending it.
#[derive(Default)]
struct DerivedCols {
    cols: Vec<ProjectItem<ColumnRef>>,
    /// Whether any column is genuinely derived. Without one the aggregate keeps
    /// its original child, so trees that lower today keep their exact shape.
    any: bool,
    /// First same-name-different-value collision, reported only if the
    /// projection is actually inserted (see [`Self::wrap`]).
    collision: Option<String>,
}

impl DerivedCols {
    /// Add `alias := expr`, or note a collision if `alias` already means
    /// something else. `Project` carries one relation qualifier for all its
    /// columns, so `a.k` and `b.k` cannot both survive it — but that only
    /// matters when a projection gets inserted at all.
    fn push(&mut self, alias: String, expr: Unresolved) {
        let existing = self
            .cols
            .iter()
            .find(|c| c.alias.as_deref() == Some(&alias));
        match existing {
            // Same name, same value — one projected column serves both uses.
            Some(e) if e.expr == expr => {}
            Some(_) => {
                self.collision.get_or_insert(alias);
            }
            None => self.cols.push(ProjectItem {
                alias: Some(alias),
                expr,
            }),
        }
    }

    /// A plain column the aggregate references — carried through unchanged.
    fn passthrough(&mut self, expr: &Expr) -> Result<(), LoweringError> {
        let Expr::Column(c) = unalias(expr) else {
            return Ok(());
        };
        self.push(c.name.clone(), df_expr_to_unresolved(expr)?);
        Ok(())
    }

    /// A genuinely derived column: `alias` now names `expr`'s value.
    fn materialize(&mut self, alias: String, expr: Unresolved) -> Result<(), LoweringError> {
        self.any = true;
        self.push(alias, expr);
        Ok(())
    }

    /// Repoint a reducer's argument at a derived column when it is an
    /// expression; otherwise carry its plain input column through.
    fn rewrite_agg(&mut self, expr: &Expr) -> Result<Expr, LoweringError> {
        let Expr::AggregateFunction(agg_fn) = unalias(expr) else {
            return Ok(expr.clone());
        };
        // `COUNT(*)` reduces no column; `agg_col_name` covers bare/aliased/cast
        // columns, so `None` here means the argument really is an expression.
        let counts_rows = agg_fn.func.name().eq_ignore_ascii_case("count") && !agg_fn.distinct;
        let Some(arg) = agg_fn.args.first() else {
            return Ok(expr.clone());
        };
        if counts_rows {
            return Ok(expr.clone());
        }
        match agg_col_name(&agg_fn.args) {
            Some(name) => {
                self.push(name, df_expr_to_unresolved(arg)?);
                Ok(expr.clone())
            }
            None => {
                let alias = unalias(arg).to_string();
                self.materialize(alias.clone(), df_expr_to_unresolved(arg)?)?;
                let mut agg_fn = agg_fn.clone();
                agg_fn.args[0] = Expr::Column(DfColumn::new_unqualified(alias));
                Ok(Expr::AggregateFunction(agg_fn))
            }
        }
    }

    /// Wrap `input` in the materializing `Project`, or return it untouched when
    /// nothing needed deriving — so a query that lowers today keeps its exact
    /// tree, and a name collision that the projection would have flattened only
    /// matters once the projection exists.
    fn wrap(self, input: Unresolved) -> Result<Unresolved, LoweringError> {
        if !self.any {
            return Ok(input);
        }
        if let Some(alias) = self.collision {
            return Err(LoweringError::UnsupportedFeature(format!(
                "ambiguous column `{alias}` beneath an expression GROUP BY / \
                 aggregate — alias the relations apart"
            )));
        }
        Ok(Unresolved::Project {
            cols: self.cols,
            qualifier: None,
            child: Rc::new(input),
        })
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

/// The single input column of a value reducer (`SUM`/`MIN`/`MAX`/`AVG`/stddev/
/// variance/quantile/count-distinct). Errors if the argument is not a column:
/// the canonical `AggIntent` reduces a column, not an arbitrary expression
/// (`SUM(a*b)`), so silently picking a probe column would compute the wrong
/// result.
fn reducer_col(name: &str, args: &[Expr]) -> Result<ColumnRef, LoweringError> {
    agg_col_name(args).map(ColumnRef::Named).ok_or_else(|| {
        LoweringError::UnsupportedAggregate(format!("{name} over a non-column expression"))
    })
}

fn expr_to_group_ref(expr: &Expr) -> Result<ColumnRef, LoweringError> {
    match expr {
        // Preserve the relation qualifier so a GROUP BY / PARTITION BY key over a
        // join (`b.k` vs `a.k`) resolves to the correct side — the same rule the
        // scalar predicate path uses (`df_expr_to_unresolved`).
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
        Some(Expr::Literal(DfScalarValue::Float64(Some(q)))) => *q,
        Some(Expr::Literal(DfScalarValue::Float32(Some(q)))) => *q as f64,
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

// ── LogicalPlan navigation helpers ──────────────────────────────────────────────

fn eval_fetch(expr_opt: &Option<Box<Expr>>) -> Option<usize> {
    expr_opt.as_ref().and_then(|e| match e.as_ref() {
        Expr::Literal(DfScalarValue::Int64(Some(v))) if *v >= 0 => Some(*v as usize),
        Expr::Literal(DfScalarValue::UInt64(Some(v))) => Some(*v as usize),
        Expr::Literal(DfScalarValue::Int32(Some(v))) if *v >= 0 => Some(*v as usize),
        _ => None,
    })
}

/// Map a DataFusion window-function definition to the canonical
/// [`WindowFuncKind`].
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
