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
        let original = physical_common::execute_raw_rows(&root, rows.clone());
        let actual = physical_common::execute_raw_rows(rewritten, rows);
        match (&original[0][0], &actual[0][0]) {
            (Value::Null, Value::Null) => {}
            (Value::Float64(a), Value::Float64(b)) => assert!((a - b).abs() < 1e-12),
            other => panic!("original SQL differs from entropy rewrite: {other:?}"),
        }

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

// Native binding refuses partial, partitioned or ordered SUM windows instead of treating them as totals.
#[tokio::test]
async fn native_sql_sum_window_rejects_other_frames() {
    use asap_physical_operators::physical_planner::bind_with_data_sources;
    use asap_physical_operators::sources::{DataSources, MemorySource};
    use asap_types::pre_asap::Source;
    use std::{collections::BTreeMap, sync::Arc};
    let schema = Schema::new(vec![Field::plain("src_ip", DataType::Int64, false)]);
    let catalog = SqlCatalog::new().with_table("flows", schema.clone());
    for sql in [
        "SELECT SUM(src_ip) OVER (PARTITION BY src_ip) FROM flows",
        "SELECT SUM(src_ip) OVER (ORDER BY src_ip) FROM flows",
        "SELECT SUM(src_ip) OVER (ROWS BETWEEN 1 PRECEDING AND CURRENT ROW) FROM flows",
    ] {
        let root = lower_sql(sql, &catalog, AccuracyTarget::Exact)
            .await
            .unwrap();
        let wire = physical_common::compile_post_asap_dag(&root).unwrap();
        let scan_schema = Arc::new(
            wire.nodes
                .iter()
                .find(|node| {
                    matches!(
                        node.payload,
                        asap_types::ir::export::PostAsapOperatorPayload::Relational {
                            operator: asap_types::ir::export::NonASAPOpKind::Scan { .. }
                        }
                    )
                })
                .unwrap()
                .output_schema
                .clone(),
        );
        let mut sources = DataSources::default();
        sources
            .register(
                Source::Table {
                    table_ref: "flows".into(),
                },
                Arc::new(MemorySource::new(scan_schema, vec![]).unwrap()),
            )
            .unwrap();
        let error =
            bind_with_data_sources(&wire, BTreeMap::new(), &[u64::from(wire.root.0)], &sources)
                .err()
                .expect("unsupported window rejected");
        assert!(
            error
                .to_string()
                .contains("native SQL window SUM requires the complete unordered relation"),
            "{error}"
        );
    }
}
