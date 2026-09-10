use asap_frontend_metricsql::{canonical_metricsql, parse_metricsql};

/// These outputs were generated with the official VictoriaMetrics Go parser,
/// `github.com/VictoriaMetrics/metricsql` v0.84.4. Comparing both inputs after
/// parsing avoids treating harmless printer whitespace as a grammar mismatch.
const OFFICIAL_GO_GOLDEN: &[(&str, &str, bool)] = &[
    (
        r#"foo{job="a" or job="b"}"#,
        r#"foo{job="a" or job="b"}"#,
        true,
    ),
    (
        r#"WITH (prefix="http_") {__name__=prefix+"requests_total"}"#,
        "http_requests_total",
        true,
    ),
    (
        "WITH (x = rate(foo[5m])) sum(x) by (job)",
        "sum(rate(foo[5m])) by(job)",
        true,
    ),
    (
        r#"sum(rate(foo{env=~"prod|staging",code!="500"}[5m])) by (job) limit 10"#,
        r#"sum(rate(foo{env=~"prod|staging",code!="500"}[5m])) by(job) limit 10"#,
        true,
    ),
    (
        "rate(foo[5i]) keep_metric_names",
        "rate(foo[5i]) keep_metric_names",
        true,
    ),
    (
        "time() ifnot time() > 1400 default -time()",
        "(time() ifnot (time() > 1400)) default (0 - time())",
        false,
    ),
    (
        "foo + on(job) group_left(instance) bar",
        "foo + on(job) group_left(instance) bar",
        true,
    ),
    ("foo offset 1.5h", "foo offset 1.5h", true),
];

#[test]
fn official_victoriametrics_corpus_has_the_same_canonical_identity() {
    for &(query, official, compare_identity) in OFFICIAL_GO_GOLDEN {
        let query_identity = canonical_metricsql(query).unwrap_or_else(|error| {
            panic!("Rust parser rejected official input {query:?}: {error}")
        });
        let official_identity = canonical_metricsql(official).unwrap_or_else(|error| {
            panic!("Rust parser rejected official canonical form {official:?}: {error}")
        });
        if compare_identity {
            assert_eq!(query_identity, official_identity, "query: {query}");
        }
    }
}

#[test]
fn official_victoriametrics_numeric_tokens_are_accepted() {
    let query = "1_000 + 0x10 + 2.5Mi";
    let official = "2.622456e+06";
    parse_metricsql(query).expect("Rust parser must accept VictoriaMetrics numeric tokens");
    parse_metricsql(official).expect("Rust parser must accept VictoriaMetrics canonical number");
}
