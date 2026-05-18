use std::collections::HashMap;
use std::rc::Rc;

use asap_control_core::intent_algebra::{
    AggIntent, ColumnRef, DataModel, GroupKey, HasSchema, L3DataType, L3Expr, L3Field, L3Node,
    L3Scalar, L3Schema, Predicate, ProjectItem, QueryExpr, SortKey, Source, TableRef, ColumnDef,
    SchemaCatalog, TableSchema,
};
use asap_control_core::types::AccuracyTarget;

// ── Helpers ───────────────────────────────────────────────────────────────────

fn field(name: &str, dtype: L3DataType) -> L3Field {
    L3Field { name: name.to_string(), dtype, nullable: false }
}

fn nullable_field(name: &str, dtype: L3DataType) -> L3Field {
    L3Field { name: name.to_string(), dtype, nullable: true }
}

fn schema(fields: Vec<L3Field>) -> L3Schema {
    L3Schema { fields, time_index: None }
}

fn schema_with_time(fields: Vec<L3Field>, time_index: usize) -> L3Schema {
    L3Schema { fields, time_index: Some(time_index) }
}

fn make_node(expr: QueryExpr, s: L3Schema) -> Rc<L3Node> {
    Rc::new(L3Node { expr, schema: s })
}

fn empty_catalog() -> SchemaCatalog {
    SchemaCatalog { tables: HashMap::new() }
}

fn metrics_catalog() -> SchemaCatalog {
    let mut tables = HashMap::new();
    tables.insert(
        "metrics".to_string(),
        TableSchema {
            columns: vec![
                ColumnDef { name: "ts".to_string(), data_type: L3DataType::Int64, nullable: false },
                ColumnDef {
                    name: "value".to_string(),
                    data_type: L3DataType::Float64,
                    nullable: true,
                },
                ColumnDef {
                    name: "region".to_string(),
                    data_type: L3DataType::Utf8,
                    nullable: true,
                },
            ],
            time_column: Some("ts".to_string()),
        },
    );
    tables.insert(
        "events".to_string(),
        TableSchema {
            columns: vec![
                ColumnDef { name: "id".to_string(), data_type: L3DataType::Int64, nullable: false },
                ColumnDef {
                    name: "name".to_string(),
                    data_type: L3DataType::Utf8,
                    nullable: true,
                },
            ],
            time_column: None,
        },
    );
    SchemaCatalog { tables }
}

fn exact() -> AccuracyTarget {
    AccuracyTarget::Exact
}

fn eps(e: f64) -> AccuracyTarget {
    AccuracyTarget::Epsilon(e)
}

// ── AggIntent::requires() ─────────────────────────────────────────────────────

#[test]
fn requires_count_is_any() {
    assert_eq!(AggIntent::Count { accuracy: exact() }.requires(), DataModel::Any);
}

#[test]
fn requires_sum_is_any() {
    assert_eq!(AggIntent::Sum.requires(), DataModel::Any);
}

#[test]
fn requires_min_is_any() {
    assert_eq!(AggIntent::Min.requires(), DataModel::Any);
}

#[test]
fn requires_max_is_any() {
    assert_eq!(AggIntent::Max.requires(), DataModel::Any);
}

#[test]
fn requires_avg_is_any() {
    assert_eq!(AggIntent::Avg.requires(), DataModel::Any);
}

#[test]
fn requires_stddev_sample_is_any() {
    assert_eq!(AggIntent::Stddev { population: false }.requires(), DataModel::Any);
}

#[test]
fn requires_stddev_population_is_any() {
    assert_eq!(AggIntent::Stddev { population: true }.requires(), DataModel::Any);
}

#[test]
fn requires_quantile_is_any() {
    assert_eq!(AggIntent::Quantile { q: 0.99, accuracy: exact() }.requires(), DataModel::Any);
}

#[test]
fn requires_cardinality_is_any() {
    assert_eq!(AggIntent::Cardinality { accuracy: exact() }.requires(), DataModel::Any);
}

#[test]
fn requires_topk_is_any() {
    assert_eq!(
        AggIntent::TopK { k: 10, by: vec![], accuracy: exact() }.requires(),
        DataModel::Any
    );
}

#[test]
fn requires_rate_is_timeseries() {
    assert_eq!(
        AggIntent::Rate { window: std::time::Duration::from_secs(60) }.requires(),
        DataModel::TimeSeries
    );
}

#[test]
fn requires_increase_is_timeseries() {
    assert_eq!(
        AggIntent::Increase { window: std::time::Duration::from_secs(300) }.requires(),
        DataModel::TimeSeries
    );
}

// ── AggIntent::output_type() ──────────────────────────────────────────────────

#[test]
fn output_type_count_is_int64() {
    let f = field("x", L3DataType::Float64);
    assert_eq!(AggIntent::Count { accuracy: exact() }.output_type(&f), L3DataType::Int64);
}

#[test]
fn output_type_count_ignores_input_type() {
    // Count is always Int64 regardless of the aggregated column's type.
    let f = field("x", L3DataType::Utf8);
    assert_eq!(AggIntent::Count { accuracy: eps(0.01) }.output_type(&f), L3DataType::Int64);
}

#[test]
fn output_type_cardinality_is_int64() {
    let f = field("host", L3DataType::Utf8);
    assert_eq!(AggIntent::Cardinality { accuracy: exact() }.output_type(&f), L3DataType::Int64);
}

#[test]
fn output_type_sum_is_float64() {
    let f = field("value", L3DataType::Float64);
    assert_eq!(AggIntent::Sum.output_type(&f), L3DataType::Float64);
}

#[test]
fn output_type_avg_is_float64() {
    let f = field("value", L3DataType::Int64);
    assert_eq!(AggIntent::Avg.output_type(&f), L3DataType::Float64);
}

#[test]
fn output_type_stddev_sample_is_float64() {
    let f = field("value", L3DataType::Float64);
    assert_eq!(AggIntent::Stddev { population: false }.output_type(&f), L3DataType::Float64);
}

#[test]
fn output_type_stddev_population_is_float64() {
    let f = field("value", L3DataType::Float64);
    assert_eq!(AggIntent::Stddev { population: true }.output_type(&f), L3DataType::Float64);
}

#[test]
fn output_type_quantile_is_float64() {
    let f = field("latency", L3DataType::Float64);
    assert_eq!(AggIntent::Quantile { q: 0.5, accuracy: exact() }.output_type(&f), L3DataType::Float64);
}

#[test]
fn output_type_rate_is_float64() {
    let f = field("bytes", L3DataType::Float64);
    assert_eq!(
        AggIntent::Rate { window: std::time::Duration::from_secs(60) }.output_type(&f),
        L3DataType::Float64
    );
}

#[test]
fn output_type_increase_is_float64() {
    let f = field("counter", L3DataType::Float64);
    assert_eq!(
        AggIntent::Increase { window: std::time::Duration::from_secs(60) }.output_type(&f),
        L3DataType::Float64
    );
}

#[test]
fn output_type_min_preserves_int64_input() {
    let f = field("count", L3DataType::Int64);
    assert_eq!(AggIntent::Min.output_type(&f), L3DataType::Int64);
}

#[test]
fn output_type_min_preserves_float64_input() {
    let f = field("value", L3DataType::Float64);
    assert_eq!(AggIntent::Min.output_type(&f), L3DataType::Float64);
}

#[test]
fn output_type_max_preserves_utf8_input() {
    let f = field("name", L3DataType::Utf8);
    assert_eq!(AggIntent::Max.output_type(&f), L3DataType::Utf8);
}

// ── HasSchema::output_schema() — Scan ─────────────────────────────────────────

#[test]
fn scan_schema_columns_from_catalog() {
    let catalog = metrics_catalog();
    let scan = QueryExpr::Scan {
        source: Source::Table {
            table_ref: TableRef("metrics".into()),
            columns: vec![],
            time_range: None,
        },
        predicates: vec![],
    };
    let s = scan.output_schema(&[], &catalog);
    assert_eq!(s.fields.len(), 3);
    assert_eq!(s.fields[0].name, "ts");
    assert_eq!(s.fields[0].dtype, L3DataType::Int64);
    assert_eq!(s.fields[1].name, "value");
    assert_eq!(s.fields[2].name, "region");
}

#[test]
fn scan_schema_time_index_set_for_time_column() {
    let catalog = metrics_catalog();
    let scan = QueryExpr::Scan {
        source: Source::Table {
            table_ref: TableRef("metrics".into()),
            columns: vec![],
            time_range: None,
        },
        predicates: vec![],
    };
    let s = scan.output_schema(&[], &catalog);
    // "ts" is at index 0 and is the time_column
    assert_eq!(s.time_index, Some(0));
}

#[test]
fn scan_schema_no_time_index_when_no_time_column() {
    let catalog = metrics_catalog();
    let scan = QueryExpr::Scan {
        source: Source::Table {
            table_ref: TableRef("events".into()),
            columns: vec![],
            time_range: None,
        },
        predicates: vec![],
    };
    let s = scan.output_schema(&[], &catalog);
    assert_eq!(s.time_index, None);
    assert_eq!(s.fields.len(), 2);
}

// ── HasSchema::output_schema() — pass-through nodes ──────────────────────────

fn child_schema() -> L3Schema {
    schema(vec![field("ts", L3DataType::Int64), nullable_field("value", L3DataType::Float64)])
}

fn dummy_scan(s: L3Schema) -> Rc<L3Node> {
    make_node(
        QueryExpr::Scan {
            source: Source::Table {
                table_ref: TableRef("metrics".into()),
                columns: vec![],
                time_range: None,
            },
            predicates: vec![],
        },
        s,
    )
}

#[test]
fn filter_passes_through_child_schema() {
    let cs = child_schema();
    let node = QueryExpr::Filter {
        child: dummy_scan(cs.clone()),
        pred: Predicate(L3Expr::Literal(L3Scalar::Boolean(true))),
    };
    let out = node.output_schema(&[&cs], &empty_catalog());
    assert_eq!(out.fields, cs.fields);
    assert_eq!(out.time_index, cs.time_index);
}

#[test]
fn sort_passes_through_child_schema() {
    let cs = child_schema();
    let node = QueryExpr::Sort {
        child: dummy_scan(cs.clone()),
        keys: vec![SortKey {
            expr: L3Expr::Column(ColumnRef("ts".into())),
            ascending: true,
            nulls_first: false,
        }],
    };
    let out = node.output_schema(&[&cs], &empty_catalog());
    assert_eq!(out.fields, cs.fields);
}

#[test]
fn limit_passes_through_child_schema() {
    let cs = child_schema();
    let node = QueryExpr::Limit { child: dummy_scan(cs.clone()), n: 10, offset: 0 };
    let out = node.output_schema(&[&cs], &empty_catalog());
    assert_eq!(out.fields, cs.fields);
}

#[test]
fn distinct_passes_through_child_schema() {
    let cs = child_schema();
    let node = QueryExpr::Distinct { child: dummy_scan(cs.clone()), cols: vec![] };
    let out = node.output_schema(&[&cs], &empty_catalog());
    assert_eq!(out.fields, cs.fields);
}

// ── HasSchema::output_schema() — Aggregate ────────────────────────────────────

fn metrics_child_schema() -> L3Schema {
    schema(vec![
        field("ts", L3DataType::Int64),
        nullable_field("value", L3DataType::Float64),
        nullable_field("region", L3DataType::Utf8),
        nullable_field("host", L3DataType::Utf8),
    ])
}

#[test]
fn aggregate_count_star_no_group_by() {
    // SELECT COUNT(*) FROM metrics
    let cs = metrics_child_schema();
    let node = QueryExpr::Aggregate {
        child: dummy_scan(cs.clone()),
        by: vec![],
        aggs: vec![AggIntent::Count { accuracy: exact() }],
        having: None,
    };
    let out = node.output_schema(&[&cs], &empty_catalog());
    assert_eq!(out.fields.len(), 1);
    assert_eq!(out.fields[0].dtype, L3DataType::Int64);
}

#[test]
fn aggregate_group_by_adds_by_cols_first() {
    // SELECT region, COUNT(*) FROM metrics GROUP BY region
    let cs = metrics_child_schema();
    let node = QueryExpr::Aggregate {
        child: dummy_scan(cs.clone()),
        by: vec![GroupKey("region".into())],
        aggs: vec![AggIntent::Count { accuracy: exact() }],
        having: None,
    };
    let out = node.output_schema(&[&cs], &empty_catalog());
    // region col + count col
    assert_eq!(out.fields.len(), 2);
    assert_eq!(out.fields[0].name, "region");
    assert_eq!(out.fields[0].dtype, L3DataType::Utf8);
    assert_eq!(out.fields[1].dtype, L3DataType::Int64);
}

#[test]
fn aggregate_multiple_aggs() {
    // SELECT COUNT(*), SUM(value), MIN(value) FROM metrics
    let cs = metrics_child_schema();
    let node = QueryExpr::Aggregate {
        child: dummy_scan(cs.clone()),
        by: vec![],
        aggs: vec![AggIntent::Count { accuracy: exact() }, AggIntent::Sum, AggIntent::Min],
        having: None,
    };
    let out = node.output_schema(&[&cs], &empty_catalog());
    assert_eq!(out.fields.len(), 3);
    assert_eq!(out.fields[0].dtype, L3DataType::Int64); // Count
    assert_eq!(out.fields[1].dtype, L3DataType::Float64); // Sum
    // Min: uses dummy Float64 input (known limitation — column not tracked at L3)
    assert_eq!(out.fields[2].dtype, L3DataType::Float64);
}

#[test]
fn aggregate_topk_produces_by_cols_plus_count() {
    // TopK { k: 5, by: [host] } → [host(Utf8), count(Int64)]
    let cs = metrics_child_schema();
    let node = QueryExpr::Aggregate {
        child: dummy_scan(cs.clone()),
        by: vec![],
        aggs: vec![AggIntent::TopK {
            k: 5,
            by: vec![ColumnRef("host".into())],
            accuracy: exact(),
        }],
        having: None,
    };
    let out = node.output_schema(&[&cs], &empty_catalog());
    assert_eq!(out.fields.len(), 2);
    assert_eq!(out.fields[0].name, "host");
    assert_eq!(out.fields[0].dtype, L3DataType::Utf8);
    assert_eq!(out.fields[1].name, "count");
    assert_eq!(out.fields[1].dtype, L3DataType::Int64);
}

#[test]
fn aggregate_topk_multi_key() {
    // TopK { k: 10, by: [region, host] } → [region(Utf8), host(Utf8), count(Int64)]
    let cs = metrics_child_schema();
    let node = QueryExpr::Aggregate {
        child: dummy_scan(cs.clone()),
        by: vec![],
        aggs: vec![AggIntent::TopK {
            k: 10,
            by: vec![ColumnRef("region".into()), ColumnRef("host".into())],
            accuracy: exact(),
        }],
        having: None,
    };
    let out = node.output_schema(&[&cs], &empty_catalog());
    assert_eq!(out.fields.len(), 3);
    assert_eq!(out.fields[0].name, "region");
    assert_eq!(out.fields[1].name, "host");
    assert_eq!(out.fields[2].name, "count");
    assert_eq!(out.fields[2].dtype, L3DataType::Int64);
}

#[test]
fn aggregate_time_index_not_propagated() {
    // Aggregating over a time-indexed child drops the time axis.
    let cs = schema_with_time(
        vec![field("ts", L3DataType::Int64), nullable_field("value", L3DataType::Float64)],
        0,
    );
    let node = QueryExpr::Aggregate {
        child: dummy_scan(cs.clone()),
        by: vec![],
        aggs: vec![AggIntent::Count { accuracy: exact() }],
        having: None,
    };
    let out = node.output_schema(&[&cs], &empty_catalog());
    assert_eq!(out.time_index, None);
}

// ── HasSchema::output_schema() — Project ──────────────────────────────────────

#[test]
fn project_column_items_derive_schema_from_child() {
    // SELECT ts, value — both columns exist in the child schema.
    let cs = child_schema(); // [ts(Int64), value(Float64)]
    let node = QueryExpr::Project {
        child: dummy_scan(cs.clone()),
        cols: vec![
            ProjectItem { expr: L3Expr::Column(ColumnRef("ts".into())), alias: None },
            ProjectItem { expr: L3Expr::Column(ColumnRef("value".into())), alias: None },
        ],
    };
    let out = node.output_schema(&[&cs], &empty_catalog());
    assert_eq!(out.fields.len(), 2);
    assert_eq!(out.fields[0].name, "ts");
    assert_eq!(out.fields[0].dtype, L3DataType::Int64);
    assert_eq!(out.fields[1].name, "value");
    assert_eq!(out.fields[1].dtype, L3DataType::Float64);
}

#[test]
fn project_alias_renames_output_field() {
    // SELECT value AS v — output field is named "v", type preserved.
    let cs = child_schema();
    let node = QueryExpr::Project {
        child: dummy_scan(cs.clone()),
        cols: vec![
            ProjectItem {
                expr: L3Expr::Column(ColumnRef("value".into())),
                alias: Some("v".into()),
            },
        ],
    };
    let out = node.output_schema(&[&cs], &empty_catalog());
    assert_eq!(out.fields.len(), 1);
    assert_eq!(out.fields[0].name, "v");
    assert_eq!(out.fields[0].dtype, L3DataType::Float64);
}

#[test]
fn project_subsets_columns() {
    // SELECT value — only one of two child columns projected.
    let cs = child_schema();
    let node = QueryExpr::Project {
        child: dummy_scan(cs.clone()),
        cols: vec![ProjectItem { expr: L3Expr::Column(ColumnRef("value".into())), alias: None }],
    };
    let out = node.output_schema(&[&cs], &empty_catalog());
    assert_eq!(out.fields.len(), 1);
    assert_eq!(out.fields[0].name, "value");
}

#[test]
fn project_preserves_time_index_when_time_col_included() {
    // SELECT ts, value — ts is at index 0 in child (time_index=0); should be preserved.
    let cs = schema_with_time(
        vec![field("ts", L3DataType::Int64), nullable_field("value", L3DataType::Float64)],
        0,
    );
    let node = QueryExpr::Project {
        child: dummy_scan(cs.clone()),
        cols: vec![
            ProjectItem { expr: L3Expr::Column(ColumnRef("ts".into())), alias: None },
            ProjectItem { expr: L3Expr::Column(ColumnRef("value".into())), alias: None },
        ],
    };
    let out = node.output_schema(&[&cs], &empty_catalog());
    assert_eq!(out.time_index, Some(0));
}

#[test]
fn project_preserves_time_index_when_col_reordered() {
    // SELECT value, ts — ts moves to index 1; time_index should update.
    let cs = schema_with_time(
        vec![field("ts", L3DataType::Int64), nullable_field("value", L3DataType::Float64)],
        0,
    );
    let node = QueryExpr::Project {
        child: dummy_scan(cs.clone()),
        cols: vec![
            ProjectItem { expr: L3Expr::Column(ColumnRef("value".into())), alias: None },
            ProjectItem { expr: L3Expr::Column(ColumnRef("ts".into())), alias: None },
        ],
    };
    let out = node.output_schema(&[&cs], &empty_catalog());
    assert_eq!(out.time_index, Some(1));
}

#[test]
fn project_drops_time_index_when_time_col_excluded() {
    // SELECT value — ts not projected; time_index should be None.
    let cs = schema_with_time(
        vec![field("ts", L3DataType::Int64), nullable_field("value", L3DataType::Float64)],
        0,
    );
    let node = QueryExpr::Project {
        child: dummy_scan(cs.clone()),
        cols: vec![ProjectItem { expr: L3Expr::Column(ColumnRef("value".into())), alias: None }],
    };
    let out = node.output_schema(&[&cs], &empty_catalog());
    assert_eq!(out.time_index, None);
}

#[test]
fn project_time_index_tracks_aliased_time_col() {
    // SELECT ts AS t — aliased; time_index should still point at the right output position.
    let cs = schema_with_time(
        vec![field("ts", L3DataType::Int64), nullable_field("value", L3DataType::Float64)],
        0,
    );
    let node = QueryExpr::Project {
        child: dummy_scan(cs.clone()),
        cols: vec![
            ProjectItem {
                expr: L3Expr::Column(ColumnRef("ts".into())),
                alias: Some("t".into()),
            },
        ],
    };
    let out = node.output_schema(&[&cs], &empty_catalog());
    assert_eq!(out.time_index, Some(0));
    assert_eq!(out.fields[0].name, "t");
}
