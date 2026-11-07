//! Relational operators and scalar expressions run end to end: each query
//! goes through its frontend and the planner's selection (exact), then the
//! executor compiles and runs the selected plan over rows bound at its
//! `Scan`s. Expected values follow the SQL and PromQL definitions.
use crate::physical_common::{compile_physical_asap_dag, raw_data_store};
use asap_executor::physical_planner::{compile, promql_rows};
use asap_executor::runtime::{Limits, RunContext, Scope};
use asap_executor::values::Value;
use asap_frontend_sql::{lower_sql, SqlCatalog};
use asap_integration_tests::fixtures::lower_promql;
use asap_integration_tests::planning::selected_dag;
use asap_types::ir::operator::Source;
use asap_types::ir::properties::AccuracyTarget;
use asap_types::ir::schema::{DataType, Field, Schema, PROMQL_SERIES_IDENTITY};
use asap_types::ir::OperatorNode;
use futures::{executor::block_on, StreamExt};
use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::Arc;

/// Run `root` (a planned or a lowered plan) at `evaluation_time_ms`, with
/// each `Scan` reading `rows(source, schema)`: the root's rows, or the
/// run's error.
fn run(
    root: &Rc<OperatorNode>,
    evaluation_time_ms: i64,
    rows: impl FnMut(&Source, &asap_executor::values::SchemaRef) -> Vec<Vec<Value>>,
) -> Result<(asap_executor::values::SchemaRef, Vec<Vec<Value>>), String> {
    let dag = compile_physical_asap_dag(root).unwrap();
    let store = raw_data_store(&dag, rows);
    let roots = [dag.roots[0] as u64];
    let plan = compile(&dag, &[], &roots).unwrap_or_else(|error| panic!("{error}"));
    let schema = Arc::new(
        dag.nodes
            .iter()
            .find(|node| node.id as u64 == roots[0])
            .unwrap()
            .output_schema
            .clone(),
    );
    let bound = plan.instantiate(Arc::new(store)).unwrap();
    let context = RunContext::new(Scope::Query { evaluation_time_ms }, Limits::default()).unwrap();
    let rows = block_on(async {
        let mut output = bound.execute(&roots, context.clone()).unwrap().remove(0);
        let mut rows = Vec::new();
        while let Some(batch) = output.next().await {
            rows.extend(batch.map_err(|error| error.to_string())?.to_rows());
        }
        Ok::<_, String>(rows)
    });
    assert_eq!(context.retained_bytes(), 0);
    Ok((schema, rows?))
}

/// Table `t(a, k, v, x, b, name)` and `u(a, x)`, with the rows each test
/// binds.
fn catalog() -> SqlCatalog {
    let field = |name: &str, dtype| Field::plain(name, dtype, true);
    SqlCatalog::new()
        .with_table(
            "t",
            Schema::new(vec![
                field("a", DataType::Utf8),
                field("k", DataType::Utf8),
                field("v", DataType::Float64),
                field("x", DataType::Float64),
                field("b", DataType::Int64),
                field("name", DataType::Utf8),
            ]),
        )
        .with_table(
            "u",
            Schema::new(vec![
                field("a", DataType::Utf8),
                field("x", DataType::Float64),
            ]),
        )
}

fn text(value: &str) -> Value {
    Value::Utf8(value.into())
}

/// Rows of `t`: (a, k, v, x, b, name).
fn t_rows() -> Vec<Vec<Value>> {
    let row = |a: &str, k: &str, v: f64, x: Option<f64>, b: i64, name: &str| {
        vec![
            text(a),
            text(k),
            Value::Float64(v),
            x.map_or(Value::Null, Value::Float64),
            Value::Int64(b),
            text(name),
        ]
    };
    vec![
        row("p", "g1", 3.0, Some(1.0), 10, "alpha"),
        row("q", "g1", 1.0, Some(5.0), 20, "Beta"),
        row("p", "g1", 3.0, Some(2.0), 30, "a_b"),
        row("r", "g2", 2.0, None, 40, "bravo"),
        row("q", "g2", 7.0, Some(9.0), -5, "Alpha"),
    ]
}

/// Rows of `u`: (a, x).
fn u_rows() -> Vec<Vec<Value>> {
    vec![
        vec![text("q"), Value::Float64(5.0)],
        vec![text("s"), Value::Null],
        vec![text("q"), Value::Float64(1.0)],
    ]
}

/// SQL `query` planned exactly (`planned`) or as lowered, run over
/// [`t_rows`] and [`u_rows`]: its rows, sorted when `sorted`.
fn sql(query: &str, planned: bool, sorted: bool) -> Vec<Vec<Value>> {
    let root = block_on(lower_sql(query, &catalog(), AccuracyTarget::exact()))
        .unwrap_or_else(|error| panic!("{query}: {error}"));
    let root = match planned {
        true => selected_dag(root, AccuracyTarget::exact()),
        false => root,
    };
    let (_, mut rows) = run(&root, 0, |source, schema| {
        let Source::Table { table_ref } = source else {
            panic!("a table")
        };
        let (names, rows): (&[&str], _) = match table_ref.as_str() {
            "t" => (&["a", "k", "v", "x", "b", "name"], t_rows()),
            "u" => (&["a", "x"], u_rows()),
            other => panic!("table {other}"),
        };
        rows.into_iter()
            .map(|row| {
                schema
                    .fields
                    .iter()
                    .map(|field| row[names.iter().position(|n| *n == field.name).unwrap()].clone())
                    .collect()
            })
            .collect()
    })
    .unwrap_or_else(|error| panic!("{query}: {error}"));
    if sorted {
        rows.sort_by_key(|row| format!("{row:?}"));
    }
    rows
}

fn int(value: i64) -> Value {
    Value::Int64(value)
}
fn float(value: f64) -> Value {
    Value::Float64(value)
}
fn assert_rows(query: &str, actual: Vec<Vec<Value>>, mut expected: Vec<Vec<Value>>, sorted: bool) {
    if sorted {
        expected.sort_by_key(|row| format!("{row:?}"));
    }
    assert_eq!(format!("{actual:?}"), format!("{expected:?}"), "{query}");
}

// `DISTINCT` keeps each distinct row once (gap #17); `UNION ALL` keeps
// both inputs' rows and `UNION` each distinct one once (#18), NULL equal
// to NULL.
#[test]
fn distinct_and_unions_follow_sql() {
    let query = "SELECT DISTINCT a FROM t";
    assert_rows(
        query,
        sql(query, true, true),
        vec![vec![text("p")], vec![text("q")], vec![text("r")]],
        true,
    );
    let query = "SELECT a FROM t UNION ALL SELECT a FROM u";
    let all = ["p", "q", "p", "r", "q", "q", "s", "q"];
    assert_rows(
        query,
        sql(query, true, true),
        all.iter().map(|a| vec![text(a)]).collect(),
        true,
    );
    let query = "SELECT x FROM t UNION SELECT x FROM u";
    assert_rows(
        query,
        sql(query, true, true),
        [None, Some(1.0), Some(2.0), Some(5.0), Some(9.0)]
            .into_iter()
            .map(|x| vec![x.map_or(Value::Null, float)])
            .collect(),
        true,
    );
}

// `GROUP BY ROLLUP` lowers to a `Concat` of one aggregate per grouping
// level (#19): every (k, a) group, every k, and the total.
#[test]
fn rollup_concatenates_its_grouping_levels() {
    let query = "SELECT k, a, sum(v) AS s FROM t GROUP BY ROLLUP(k, a)";
    let row = |k: Option<&str>, a: Option<&str>, s: f64| {
        vec![
            k.map_or(Value::Null, text),
            a.map_or(Value::Null, text),
            float(s),
        ]
    };
    assert_rows(
        query,
        sql(query, true, true),
        vec![
            row(Some("g1"), Some("p"), 6.0),
            row(Some("g1"), Some("q"), 1.0),
            row(Some("g2"), Some("r"), 2.0),
            row(Some("g2"), Some("q"), 7.0),
            row(Some("g1"), None, 7.0),
            row(Some("g2"), None, 9.0),
            row(None, None, 16.0),
        ],
        true,
    );
}

// SQL window functions (#20): per partition `k`, ordered by `v` (ties
// are peers): `row_number` counts rows, `rank` skips after ties,
// `dense_rank` does not; the default frame (`RANGE UNBOUNDED PRECEDING
// AND CURRENT ROW`) sums up to the last peer; `lag` reads the previous
// row or its default; a `ROWS` frame counts rows.
#[test]
fn window_functions_follow_sql() {
    // Partition g1 by v: q (v 1, b 20), then the peers p (3, 10) and p (3, 30);
    // g2: r (2, 40), q (7, -5). Each function's value per row, by b.
    for (function, by_b) in [
        (
            "row_number() OVER (PARTITION BY k ORDER BY v, b)",
            [2, 1, 3, 1, 2].map(int),
        ),
        (
            "rank() OVER (PARTITION BY k ORDER BY v)",
            [2, 1, 2, 1, 2].map(int),
        ),
        (
            "dense_rank() OVER (PARTITION BY k ORDER BY v DESC)",
            [1, 2, 1, 2, 1].map(int),
        ),
        (
            "sum(v) OVER (PARTITION BY k ORDER BY v)",
            [7.0, 1.0, 7.0, 2.0, 9.0].map(float),
        ),
        (
            "lag(b, 1, 0) OVER (PARTITION BY k ORDER BY b)",
            [0, 10, 20, -5, 0].map(int),
        ),
        (
            "count(*) OVER (PARTITION BY k ORDER BY b ROWS BETWEEN 1 PRECEDING AND CURRENT ROW)",
            [1, 2, 2, 2, 1].map(int),
        ),
    ] {
        let query = format!("SELECT b, {function} AS w FROM t");
        assert_rows(
            &query,
            sql(&query, true, true),
            [10, 20, 30, 40, -5]
                .into_iter()
                .zip(by_b)
                .map(|(b, w)| vec![int(b), w])
                .collect(),
            true,
        );
    }
}

// Scalar subqueries, `IN` and `EXISTS` bind the subquery's value (#26).
// `x > (SELECT avg(x) FROM t)` keeps the rows above the mean of the
// non-null x (17 / 4); `a IN (SELECT a FROM u) OR x > 8` keeps a's in u
// and x above 8.
#[test]
fn subqueries_bind_their_values() {
    // Stage 3 selects no plan for a scalar read of an aggregate (#1243),
    // so this one runs its lowered (exact) plan.
    let query = "SELECT a, x FROM t WHERE x > (SELECT avg(x) FROM t)";
    assert_rows(
        query,
        sql(query, false, true),
        vec![vec![text("q"), float(5.0)], vec![text("q"), float(9.0)]],
        true,
    );
    let query = "SELECT a, b FROM t WHERE a IN (SELECT a FROM u) OR x > 8";
    assert_rows(
        query,
        sql(query, true, true),
        vec![vec![text("q"), int(20)], vec![text("q"), int(-5)]],
        true,
    );
    // An uncorrelated `EXISTS` in a disjunction is one bound value.
    let query = "SELECT a FROM t WHERE EXISTS (SELECT a FROM u WHERE x > 100) OR b = 40";
    assert_rows(query, sql(query, true, true), vec![vec![text("r")]], true);
    // `NOT (x IN (…))` over a subquery with a NULL is NULL or false for
    // every row: only `b = 40` keeps one.
    let query = "SELECT a FROM t WHERE NOT (x IN (SELECT x FROM u)) OR b = 40";
    assert_rows(query, sql(query, true, true), vec![vec![text("r")]], true);
}

// Local scalars (#27): `LIKE` is case-sensitive with `_` one character,
// `ILIKE` is not; `CASE k WHEN …` compares the operand; casts follow
// DataFusion (a float truncates to an integer).
#[test]
fn local_scalars_follow_sql() {
    let query = "SELECT name FROM t WHERE name LIKE 'a%'";
    assert_rows(
        query,
        sql(query, true, true),
        vec![vec![text("a_b")], vec![text("alpha")]],
        true,
    );
    let query = "SELECT name FROM t WHERE name ILIKE 'a%' AND name NOT LIKE 'a\\_%'";
    assert_rows(
        query,
        sql(query, true, true),
        vec![vec![text("Alpha")], vec![text("alpha")]],
        true,
    );
    let query = "SELECT b, CASE k WHEN 'g1' THEN 1 WHEN 'g2' THEN 2 ELSE 0 END AS c, \
        CAST(b AS VARCHAR) AS s, CAST(v * 1.5 AS BIGINT) AS i FROM t";
    assert_rows(
        query,
        sql(query, true, true),
        vec![
            vec![int(10), int(1), text("10"), int(4)],
            vec![int(20), int(1), text("20"), int(1)],
            vec![int(30), int(1), text("30"), int(4)],
            vec![int(40), int(2), text("40"), int(3)],
            vec![int(-5), int(2), text("-5"), int(10)],
        ],
        true,
    );
}

// A sort key that is an expression is computed before sorting (#28): the
// SQL frontend does not project it.
#[test]
fn sort_by_an_expression() {
    let query = "SELECT a, b FROM t ORDER BY v * 10 + b DESC, a";
    // v * 10 + b: 40, 30, 60, 60, 65.
    assert_rows(
        query,
        sql(query, true, false),
        vec![
            vec![text("q"), int(-5)],
            vec![text("p"), int(30)],
            vec![text("r"), int(40)],
            vec![text("p"), int(10)],
            vec![text("q"), int(20)],
        ],
        false,
    );
}

/// A PromQL sample or answer: its series' labels and its value.
type Sample = (BTreeMap<String, String>, f64);

/// Evaluation time of the PromQL queries: 2023-11-14T22:13:20Z.
const EVALUATION_MS: i64 = 1_700_000_000_000;

/// A sample of a series, 0.5 s before [`EVALUATION_MS`].
fn sample(labels: &[(&str, &str)], value: f64) -> Sample {
    (
        labels
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
        value,
    )
}

/// PromQL `query` lowered with series identity where it has one (as the
/// planner's facade does), planned exactly (`planned`) or as lowered, run
/// over `samples`: each answer's labels and value, by labels.
fn promql(query: &str, samples: &[Sample], planned: bool) -> Vec<Sample> {
    try_promql(query, samples, planned).unwrap_or_else(|error| panic!("{query}: {error}"))
}

/// [`promql`], or the run's error.
fn try_promql(query: &str, samples: &[Sample], planned: bool) -> Result<Vec<Sample>, String> {
    let root = lower_promql(query, AccuracyTarget::exact()).unwrap();
    let root = promql_rows::with_series_identity(&root).unwrap_or(root);
    let root = match planned {
        true => selected_dag(root, AccuracyTarget::exact()),
        false => root,
    };
    let (schema, rows) = run(&root, EVALUATION_MS, |source, schema| {
        let Source::TimeSeries { metric } = source else {
            panic!("a time series")
        };
        samples
            .iter()
            .filter(|(labels, _)| labels["__name__"] == *metric)
            .map(|(labels, value)| {
                promql_rows::series_row(schema, labels, EVALUATION_MS - 500, *value).unwrap()
            })
            .collect()
    })?;
    let mut answers: Vec<_> = rows
        .iter()
        .map(|row| {
            let mut labels = BTreeMap::new();
            let mut value = None;
            for (field, cell) in schema.fields.iter().zip(row) {
                match cell {
                    Value::Utf8(text) if field.name == PROMQL_SERIES_IDENTITY => {
                        labels.extend(promql_rows::decode_series_identity(text).unwrap())
                    }
                    Value::Utf8(text) if !text.is_empty() => {
                        labels.insert(field.name.clone(), text.to_string());
                    }
                    Value::Float64(v) => value = Some(*v),
                    _ => {}
                }
            }
            (labels, value.expect("a sample value"))
        })
        .collect();
    answers.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(answers)
}

fn labels(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    sample(pairs, 0.0).0
}

// `vector(s)` is one label-less sample (#21); `hour()` reads the
// evaluation time; `time()` is the evaluation time in seconds (#27).
#[test]
fn vector_and_time_follow_promql() {
    assert_eq!(promql("vector(1)", &[], true), [(labels(&[]), 1.0)]);
    assert_eq!(promql("hour()", &[], true), [(labels(&[]), 22.0)]);
    let m = [sample(&[("__name__", "m"), ("i", "a")], 1_700_000_100.0)];
    assert_eq!(
        promql("m - time()", &m, true),
        [(labels(&[("i", "a")]), 100.0)]
    );
}

// `label_replace` writes the expanded replacement where its anchored regex
// matches the source label, and leaves other series unchanged; an empty
// result removes the label. `label_join` joins its sources, an absent one
// as empty (#22).
#[test]
fn label_rewrites_follow_promql() {
    let m = [
        sample(&[("__name__", "m"), ("src", "abc")], 1.0),
        sample(&[("__name__", "m"), ("src", "xabc")], 2.0),
        sample(&[("__name__", "m"), ("src", "a")], 3.0),
    ];
    assert_eq!(
        promql(
            r#"label_replace(m, "dst", "$1-x", "src", "a(b.*)")"#,
            &m,
            true
        ),
        [
            (
                labels(&[("__name__", "m"), ("dst", "bc-x"), ("src", "abc")]),
                1.0
            ),
            (labels(&[("__name__", "m"), ("src", "a")]), 3.0),
            (labels(&[("__name__", "m"), ("src", "xabc")]), 2.0),
        ]
    );
    assert_eq!(
        promql(r#"label_replace(m, "src", "", "src", "a")"#, &m, true),
        [
            (labels(&[("__name__", "m")]), 3.0),
            (labels(&[("__name__", "m"), ("src", "abc")]), 1.0),
            (labels(&[("__name__", "m"), ("src", "xabc")]), 2.0),
        ]
    );
    // Two series that end with one label set are an error, as in Prometheus.
    let error = try_promql(r#"label_replace(m, "src", "", "src", "a.*")"#, &m, true).unwrap_err();
    assert!(error.contains("same labelset"), "{error}");
    let m = [
        sample(&[("__name__", "m"), ("a", "x"), ("b", "y")], 1.0),
        sample(&[("__name__", "m"), ("b", "z")], 2.0),
    ];
    assert_eq!(
        promql(r#"label_join(m, "dst", ",", "a", "b")"#, &m, true),
        [
            (
                labels(&[("__name__", "m"), ("a", "x"), ("b", "y"), ("dst", "x,y")]),
                1.0
            ),
            (labels(&[("__name__", "m"), ("b", "z"), ("dst", ",z")]), 2.0),
        ]
    );
}

// `limitk(k, v)` keeps each group's first k series in label order;
// `limit_ratio(r, v)` and `limit_ratio(r - 1, v)` keep complementary
// series, deterministically (#24).
#[test]
fn series_sampling_follows_promql() {
    let m: Vec<_> = ["d", "b", "a", "c", "e"]
        .iter()
        .map(|i| sample(&[("__name__", "m"), ("i", i)], 1.0))
        .collect();
    let kept = |query: &str| -> Vec<String> {
        promql(query, &m, true)
            .into_iter()
            .map(|(labels, _)| labels["i"].clone())
            .collect()
    };
    assert_eq!(kept("limitk(2, m)"), ["a", "b"]);
    assert_eq!(kept("limitk(0, m)"), Vec::<String>::new());
    let half = kept("limit_ratio(0.5, m)");
    let rest = kept("limit_ratio(-0.5, m)");
    assert_eq!(half, kept("limit_ratio(0.5, m)"));
    let mut all = [half.clone(), rest.clone()].concat();
    all.sort();
    assert_eq!(all, ["a", "b", "c", "d", "e"], "{half:?} and {rest:?}");
    assert_eq!(kept("limit_ratio(1, m)").len(), 5);
}

// `v op scalar(w)` reads `w`'s one sample, NaN unless it has exactly one
// (#26).
#[test]
fn scalar_of_a_vector_binds_its_value() {
    let samples = [
        sample(&[("__name__", "m"), ("i", "a")], 1.0),
        sample(&[("__name__", "m"), ("i", "b")], 5.0),
        sample(&[("__name__", "n"), ("j", "x")], 2.0),
    ];
    assert_eq!(
        promql("m + scalar(n)", &samples, true),
        [(labels(&[("i", "a")]), 3.0), (labels(&[("i", "b")]), 7.0)]
    );
    // Two series of `m`: scalar(m) is NaN.
    let answers = promql("n + scalar(m)", &samples, true);
    assert!(answers.len() == 1 && answers[0].1.is_nan(), "{answers:?}");
    // Stage 3 selects no plan for a scalar read of an aggregate
    // (#1243): the lowered plan runs.
    assert_eq!(
        promql("m > scalar(sum(n))", &samples, false),
        [(labels(&[("__name__", "m"), ("i", "b")]), 5.0)]
    );
}
