//! SQL relational operators and scalar expressions run end to end: each
//! query goes through the SQL frontend and the planner's selection
//! (exact), then the executor compiles and runs the selected plan over rows
//! bound at its `Scan`s. Expected values follow SQL (DuckDB on the same
//! rows).
use crate::physical_common::run_selected_root as run;
use asap_executor::values::Value;
use asap_frontend_sql::{lower_sql, SqlCatalog};
use asap_integration_tests::planning::selected_dag;
use asap_types::accuracy::AccuracyTarget;
use asap_types::ir::operator::Source;
use asap_types::ir::schema::{DataType, Field, Schema};
use futures::executor::block_on;

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

/// SQL `query` planned exactly, run over [`t_rows`] and [`u_rows`]: its
/// rows, sorted when `sorted`.
fn sql(query: &str, sorted: bool) -> Vec<Vec<Value>> {
    let root =
        block_on(lower_sql(query, &catalog())).unwrap_or_else(|error| panic!("{query}: {error}"));
    let root = selected_dag(root, AccuracyTarget::exact());
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

// `DISTINCT` keeps each distinct row once; `UNION ALL` keeps
// both inputs' rows and `UNION` each distinct one once, NULL equal
// to NULL.
#[test]
fn distinct_and_unions_follow_sql() {
    let query = "SELECT DISTINCT a FROM t";
    assert_rows(
        query,
        sql(query, true),
        vec![vec![text("p")], vec![text("q")], vec![text("r")]],
        true,
    );
    let query = "SELECT a FROM t UNION ALL SELECT a FROM u";
    let all = ["p", "q", "p", "r", "q", "q", "s", "q"];
    assert_rows(
        query,
        sql(query, true),
        all.iter().map(|a| vec![text(a)]).collect(),
        true,
    );
    let query = "SELECT x FROM t UNION SELECT x FROM u";
    assert_rows(
        query,
        sql(query, true),
        [None, Some(1.0), Some(2.0), Some(5.0), Some(9.0)]
            .into_iter()
            .map(|x| vec![x.map_or(Value::Null, float)])
            .collect(),
        true,
    );
}

// `GROUP BY ROLLUP` lowers to a `Concat` of one aggregate per grouping
// level: every (k, a) group, every k, and the total.
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
        sql(query, true),
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

// SQL window functions: per partition `k`, ordered by `v` (ties
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
            sql(&query, true),
            [10, 20, 30, 40, -5]
                .into_iter()
                .zip(by_b)
                .map(|(b, w)| vec![int(b), w])
                .collect(),
            true,
        );
    }
}

// Scalar subqueries, `IN` and `EXISTS` bind the subquery's value.
// `x > (SELECT avg(x) FROM t)` keeps the rows above the mean of the
// non-null x (17 / 4); `a IN (SELECT a FROM u) OR x > 8` keeps a's in u
// and x above 8.
#[test]
fn subqueries_bind_their_values() {
    let query = "SELECT a, x FROM t WHERE x > (SELECT avg(x) FROM t)";
    assert_rows(
        query,
        sql(query, true),
        vec![vec![text("q"), float(5.0)], vec![text("q"), float(9.0)]],
        true,
    );
    let query = "SELECT a, b FROM t WHERE a IN (SELECT a FROM u) OR x > 8";
    assert_rows(
        query,
        sql(query, true),
        vec![vec![text("q"), int(20)], vec![text("q"), int(-5)]],
        true,
    );
    // An uncorrelated `EXISTS` in a disjunction is one bound value.
    let query = "SELECT a FROM t WHERE EXISTS (SELECT a FROM u WHERE x > 100) OR b = 40";
    assert_rows(query, sql(query, true), vec![vec![text("r")]], true);
    // `NOT (x IN (…))` over a subquery with a NULL is NULL or false for
    // every row: only `b = 40` keeps one.
    let query = "SELECT a FROM t WHERE NOT (x IN (SELECT x FROM u)) OR b = 40";
    assert_rows(query, sql(query, true), vec![vec![text("r")]], true);
}

// Several reads in one expression: each binds after the ones before it, an
// `IN` probe reading the input's own columns. `a IN (SELECT a FROM u)`
// keeps the q rows; `b IN (SELECT CAST(x AS BIGINT) FROM u)` (5, NULL, 1)
// matches no b; `x IN (SELECT x FROM u)` is true for x 1 and 5, NULL for
// the rest (u has a NULL x).
#[test]
fn several_subquery_reads_bind_in_turn() {
    let q_rows = vec![vec![text("q"), int(20)], vec![text("q"), int(-5)]];
    for query in [
        "SELECT a, b FROM t WHERE x > (SELECT avg(x) FROM t) OR a IN (SELECT a FROM u)",
        "SELECT a, b FROM t WHERE a IN (SELECT a FROM u) OR b IN (SELECT CAST(x AS BIGINT) FROM u)",
    ] {
        assert_rows(query, sql(query, true), q_rows.clone(), true);
    }
    let query = "SELECT a, x IN (SELECT x FROM u) AS m1, a IN (SELECT a FROM u) AS m2 FROM t";
    let row = |a: &str, m1: Option<bool>, m2: bool| {
        vec![
            text(a),
            m1.map_or(Value::Null, Value::Bool),
            Value::Bool(m2),
        ]
    };
    assert_rows(
        query,
        sql(query, true),
        vec![
            row("p", Some(true), false),
            row("q", Some(true), true),
            row("p", None, false),
            row("r", None, false),
            row("q", None, true),
        ],
        true,
    );
}

// Local scalars: `LIKE` is case-sensitive with `_` one character,
// `ILIKE` is not; `CASE k WHEN …` compares the operand; casts follow
// DataFusion (a float truncates to an integer).
#[test]
fn local_scalars_follow_sql() {
    let query = "SELECT name FROM t WHERE name LIKE 'a%'";
    assert_rows(
        query,
        sql(query, true),
        vec![vec![text("a_b")], vec![text("alpha")]],
        true,
    );
    let query = "SELECT name FROM t WHERE name ILIKE 'a%' AND name NOT LIKE 'a\\_%'";
    assert_rows(
        query,
        sql(query, true),
        vec![vec![text("Alpha")], vec![text("alpha")]],
        true,
    );
    let query = "SELECT b, CASE k WHEN 'g1' THEN 1 WHEN 'g2' THEN 2 ELSE 0 END AS c, \
        CAST(b AS VARCHAR) AS s, CAST(v * 1.5 AS BIGINT) AS i FROM t";
    assert_rows(
        query,
        sql(query, true),
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

// A sort key that is an expression is computed before sorting: the
// SQL frontend does not project it.
#[test]
fn sort_by_an_expression() {
    let query = "SELECT a, b FROM t ORDER BY v * 10 + b DESC, a";
    // v * 10 + b: 40, 30, 60, 60, 65.
    assert_rows(
        query,
        sql(query, false),
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
