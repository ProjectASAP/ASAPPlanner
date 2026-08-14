//! SQL → Layer-2 relational lowering.
//!
//! Parses SQL via DataFusion (over the catalog's registered tables), then walks
//! the unoptimized `LogicalPlan` and emits the language-independent
//! [`relational::QueryExpr`](asap_l2::relational) that
//! [`convert_root`](asap_l2::convert_root) lowers to
//! canonical L3. Positional column identity, accuracy threading, and the
//! window-over-aggregate fold all happen in that converter — this front end
//! only interprets SQL semantics into the shared L2 algebra.

use std::sync::Arc;

use datafusion::catalog_common::MemorySchemaProvider;
use datafusion::common::tree_node::{Transformed, TreeNode, TreeNodeRecursion};
use datafusion::common::{Column as DfColumn, ScalarValue};
use datafusion::datasource::MemTable;
use datafusion::logical_expr::{
    self, Distinct, Expr, JoinType, LogicalPlan, WindowFunctionDefinition,
};
use datafusion::prelude::{SessionConfig, SessionContext};

use asap_l2::relational::{
    AggFunc, AggItem, L2ProjectItem, L2SortKey, QueryExpr as L2, SourceSpec,
};
use asap_types::intent_algebra::schema::{DataType, Schema};
use asap_types::intent_algebra::{
    ColumnRef, CompareOp, JoinKind, L2Expr, L3Scalar, SetOpKind, WindowFuncKind,
};
use asap_types::workload::SqlDialect;

use crate::error::SqlError as LoweringError;

mod expr;
mod types;

pub use types::SqlCatalog;

use self::expr::df_expr_to_l2;
use self::types::{arrow_to_l3, schema_to_arrow};

/// Lowers SQL strings to the Layer-2 [`relational::QueryExpr`] over a table
/// [`SqlCatalog`]. Call [`convert_root`](asap_l2::convert_root)
/// on the result for canonical L3.
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
    /// only changes *parsing*: ClickHouse-only builtin functions (`uniqExact`,
    /// `countIf`, …) are still unknown to DataFusion's planner and still fail
    /// there, and `ElasticSQL` has no vendored parser at all.
    pub fn with_dialect(catalog: &'a SqlCatalog, dialect: SqlDialect) -> Self {
        Self { catalog, dialect }
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
        Ok(ctx)
    }

    fn lower_plan(&self, plan: &LogicalPlan) -> Result<L2, LoweringError> {
        match plan {
            LogicalPlan::TableScan(scan) => self.lower_table_scan(scan),
            LogicalPlan::Filter(filter) => self.lower_filter(filter),
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
                            L2::Project { cols, input, .. } => Ok(L2::Project {
                                cols,
                                qualifier: Some(alias_name),
                                input,
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
                                    .map(|f| L2ProjectItem {
                                        alias: Some(f.name().clone()),
                                        expr: L2Expr::Column(ColumnRef::Named(f.name().clone())),
                                    })
                                    .collect();
                                Ok(L2::Project {
                                    cols,
                                    qualifier: Some(alias_name),
                                    input: Box::new(inner),
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
    /// and keeping `Filter` directly over the `Source` preserves the converter's
    /// fold of predicates onto the `Scan`.
    fn lower_filter(&self, filter: &logical_expr::Filter) -> Result<L2, LoweringError> {
        let mut conjuncts = Vec::new();
        split_conjunction(&filter.predicate, &mut conjuncts);
        let (subqueries, residual): (Vec<_>, Vec<_>) = conjuncts
            .into_iter()
            .partition(|e| matches!(e, Expr::InSubquery(_) | Expr::Exists(_)));

        let input = self.lower_plan(&filter.input)?;
        let mut node = match rebuild_conjunction(&residual) {
            Some(pred) => L2::Filter {
                pred: df_expr_to_l2(&pred)?,
                input: Box::new(input),
            },
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
        left: L2,
    ) -> Result<L2, LoweringError> {
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
            LogicalPlan::Projection(p) if p.expr.len() == 1 => L2::Project {
                cols: vec![L2ProjectItem {
                    alias: Some(IN_SUBQUERY_KEY.to_string()),
                    expr: df_expr_to_l2(unalias(&p.expr[0]))?,
                }],
                qualifier: None,
                input: Box::new(self.lower_plan(&p.input)?),
            },
            other => L2::Project {
                cols: vec![L2ProjectItem {
                    alias: Some(IN_SUBQUERY_KEY.to_string()),
                    expr: L2Expr::Column(ColumnRef::Named(key.name().clone())),
                }],
                qualifier: None,
                input: Box::new(self.lower_plan(other)?),
            },
        };
        Ok(L2::Join {
            kind: JoinKind::Semi,
            pred: Some(L2Expr::Compare {
                left: Box::new(df_expr_to_l2(&is.expr)?),
                op: CompareOp::Eq,
                right: Box::new(L2Expr::Column(ColumnRef::Named(
                    IN_SUBQUERY_KEY.to_string(),
                ))),
            }),
            left: Box::new(left),
            right: Box::new(right),
        })
    }

    /// `[NOT] EXISTS (SELECT … WHERE inner.k = outer.k)` → a semi- / anti-join
    /// on the correlation predicate (issue #111).
    fn lower_exists(&self, ex: &logical_expr::expr::Exists, left: L2) -> Result<L2, LoweringError> {
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
        let pred = correlation.map(|e| df_expr_to_l2(&e)).transpose()?;
        Ok(L2::Join {
            kind,
            pred,
            left: Box::new(left),
            right: Box::new(right),
        })
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
        Ok(L2::Project {
            cols,
            qualifier: None,
            input,
        })
    }

    fn lower_aggregate(&self, agg: &logical_expr::Aggregate) -> Result<L2, LoweringError> {
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
                    derived.materialize(name.clone(), df_expr_to_l2(other)?)?;
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

        let input = Box::new(derived.wrap(input)?);
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
        let aggs = aggr_expr
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
            // SQL `GROUP BY` is always an inclusion list — there is no `without`.
            without: false,
            aggs,
            input,
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
        input: L2,
    ) -> Result<L2, LoweringError> {
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
            .map(|f| Ok((f.name().to_string(), arrow_to_l3(f.data_type())?)))
            .collect::<Result<_, LoweringError>>()?;

        let out_names: Vec<String> = agg
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
        let aggs = aggr_expr
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
        let input = derived.wrap(input)?;

        let branches = expand_grouping_set(gs)
            .iter()
            .map(|level| {
                let level_keys = distinct
                    .iter()
                    .filter(|e| level.contains(e))
                    .map(|e| expr_to_group_ref(e))
                    .collect::<Result<Vec<_>, LoweringError>>()?;
                let aggregate = L2::Aggregate {
                    keys: level_keys,
                    without: false,
                    aggs: aggs.clone(),
                    input: Box::new(input.clone()),
                };
                // Reinstate omitted keys as typed nulls, in canonical order.
                let cols = keys
                    .iter()
                    .zip(&distinct)
                    .map(|((name, dtype), e)| L2ProjectItem {
                        alias: Some(name.clone()),
                        expr: if level.contains(e) {
                            L2Expr::Column(ColumnRef::Named(name.clone()))
                        } else {
                            L2Expr::Cast {
                                expr: Box::new(L2Expr::Literal(L3Scalar::Null)),
                                to: dtype.clone(),
                                try_cast: false,
                            }
                        },
                    })
                    .chain(out_names.iter().map(|n| L2ProjectItem {
                        alias: Some(n.clone()),
                        expr: L2Expr::Column(ColumnRef::Named(n.clone())),
                    }))
                    .collect();
                Ok(L2::Project {
                    cols,
                    qualifier: None,
                    input: Box::new(aggregate),
                })
            })
            .collect::<Result<Vec<_>, LoweringError>>()?;

        Ok(L2::Merge { inputs: branches })
    }

    fn lower_sort(&self, sort: &logical_expr::Sort) -> Result<L2, LoweringError> {
        // A count-ranked `ORDER BY … LIMIT k` is the frequency heavy-hitter the
        // `TopK` intent represents, but that promotion now happens in the shared
        // L3 `canonicalize` pass (issue #34) — the same one both front ends run —
        // so SQL emits a plain `Sort` (+ `Limit`) here and lets canonicalization
        // recognise the count-ranked shape positionally. This removes the gate's
        // alias blind spot (#20).
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
        // Count-ranked `LIMIT k` over a `Sort` is promoted to the heavy-hitter
        // `TopK` by the shared L3 `canonicalize` pass (issue #34), not here.
        Ok(L2::Limit {
            n: eval_fetch(&limit.fetch).unwrap_or(usize::MAX) as u64,
            offset: eval_fetch(&limit.skip).unwrap_or(0) as u64,
            input: Box::new(self.lower_plan(&limit.input)?),
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
            // is rejected, not silently reduced over a probe column. Quantile
            // and CountDistinct reduce a column too, so they take the same path:
            // at L3 their `col` is `Option<ColumnId>` where `None` means "the
            // PromQL sample value", which a SQL query never has. Taking an
            // expression here would set `col: None` and silently drop it (#115).
            let (func, col) = match name.as_str() {
                "count" if agg_fn.distinct => {
                    (AggFunc::CountDistinct, reducer_col(&name, &agg_fn.args)?)
                }
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
                    reducer_col(&name, &agg_fn.args)?,
                ),
                // `median(c)` is the φ=0.5 quantile. As with `approx_distinct` and
                // `approx_percentile_cont`, the `approx_` prefix does not force an
                // approximation: the sketch-vs-exact choice is the AccuracyTarget's
                // (see `plan::boundary`), so both spellings share one intent (#111).
                "median" | "approx_median" => {
                    (AggFunc::Quantile(0.5), reducer_col(&name, &agg_fn.args)?)
                }
                "approx_distinct" => (AggFunc::CountDistinct, reducer_col(&name, &agg_fn.args)?),
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

/// The name an `IN (subquery)`'s key column is projected under, so the join
/// predicate cannot bind it to a same-named column of the outer relation.
const IN_SUBQUERY_KEY: &str = "__asap_in_key";

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
    cols: Vec<L2ProjectItem>,
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
    fn push(&mut self, alias: String, expr: L2Expr) {
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
            None => self.cols.push(L2ProjectItem {
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
        self.push(c.name.clone(), df_expr_to_l2(expr)?);
        Ok(())
    }

    /// A genuinely derived column: `alias` now names `expr`'s value.
    fn materialize(&mut self, alias: String, expr: L2Expr) -> Result<(), LoweringError> {
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
                self.push(name, df_expr_to_l2(arg)?);
                Ok(expr.clone())
            }
            None => {
                let alias = unalias(arg).to_string();
                self.materialize(alias.clone(), df_expr_to_l2(arg)?)?;
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
    fn wrap(self, input: L2) -> Result<L2, LoweringError> {
        if !self.any {
            return Ok(input);
        }
        if let Some(alias) = self.collision {
            return Err(LoweringError::UnsupportedFeature(format!(
                "ambiguous column `{alias}` beneath an expression GROUP BY / \
                 aggregate — alias the relations apart"
            )));
        }
        Ok(L2::Project {
            cols: self.cols,
            qualifier: None,
            input: Box::new(input),
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
/// L3 reduces a column, not an arbitrary expression (`SUM(a*b)`), so silently
/// picking a probe column would compute the wrong result.
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

// ── LogicalPlan navigation helpers ──────────────────────────────────────────────

fn eval_fetch(expr_opt: &Option<Box<Expr>>) -> Option<usize> {
    expr_opt.as_ref().and_then(|e| match e.as_ref() {
        Expr::Literal(ScalarValue::Int64(Some(v))) if *v >= 0 => Some(*v as usize),
        Expr::Literal(ScalarValue::UInt64(Some(v))) => Some(*v as usize),
        Expr::Literal(ScalarValue::Int32(Some(v))) if *v >= 0 => Some(*v as usize),
        _ => None,
    })
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
