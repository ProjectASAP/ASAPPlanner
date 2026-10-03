//! The proposal's entropy idiom executes in native exact frequency operators, with SQL units.
mod physical_common;
use asap_aware_mapping::{
    replacement::{Replacement, ReplacementStrategy, TargetSubDAG},
    SemanticEquivalentRewriteStrategy,
};
use asap_frontend_sql::{lower_sql, SqlCatalog};
use asap_physical_operators::values::Value;
use asap_types::{
    pre_asap::{DataType, Field, Schema},
    types::AccuracyTarget,
};

// Native wire execution produces nats, NULL for no population and SQL's negative zero for one identity.
#[tokio::test]
async fn entropy_rewrite_executes_nats_and_empty_population_guard() {
    let catalog = SqlCatalog::new().with_table(
        "flows",
        Schema::new(vec![
            Field::plain("src_ip", DataType::Utf8, false),
            Field::plain("keep", DataType::Bool, false),
        ]),
    );
    let root = lower_sql("SELECT -SUM(p*LN(p)) AS entropy FROM (SELECT COUNT(*)*1.0/SUM(COUNT(*)) OVER () AS p FROM flows WHERE keep GROUP BY src_ip) f", &catalog, AccuracyTarget::Exact).await.unwrap();
    let candidates = SemanticEquivalentRewriteStrategy.replacements(&TargetSubDAG::new(&root));
    let Replacement::SubDAG(rewritten) = &candidates[0].replacement else {
        panic!("entropy rewrite");
    };
    for (keys, expected) in [
        (vec![], None),
        (vec!["a", "a"], Some(-0.0)),
        (vec!["a", "a", "b", "b"], Some(std::f64::consts::LN_2)),
        (
            vec!["a", "a", "a", "b"],
            Some(-0.75_f64 * 0.75_f64.ln() - 0.25_f64 * 0.25_f64.ln()),
        ),
    ] {
        let mut rows: Vec<_> = keys
            .into_iter()
            .map(|key| vec![Value::Utf8(key.into()), Value::Bool(true)])
            .collect();
        rows.push(vec![Value::Utf8("discard".into()), Value::Bool(false)]);
        let actual = physical_common::execute_raw_rows(rewritten, rows);
        match (&actual[0][0], expected) {
            (Value::Null, None) => {}
            (Value::Float64(value), Some(expected)) => {
                assert!((value - expected).abs() < 1e-12);
                if expected == 0.0 {
                    assert_eq!(value.to_bits(), (-0.0_f64).to_bits());
                }
            }
            other => panic!("wrong SQL entropy: {other:?}"),
        }
    }
}
