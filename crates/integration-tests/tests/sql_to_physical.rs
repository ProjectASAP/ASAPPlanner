//! SQL frontend, candidate selection, physical compilation and fresh-run execution.
mod physical_common;
use asap_aware_mapping::{search_workload, DefaultCostModel};
use asap_frontend_sql::{lower_sql, SqlCatalog};
use asap_physical_operators::{
    physical_planner::{compile, InputContract, Source},
    runtime::{Limits, RunContext, Scope},
    sources::{DataSources, MemorySource},
    values::{Batch, Value},
};
use asap_types::ir::physical_export::PhysicalASAPOperatorPayload;
use asap_types::{
    post_asap::FieldDataType,
    pre_asap::{DataType, Field, Schema},
    types::AccuracyTarget,
};
use futures::StreamExt;
use physical_common::compile_physical_asap_dag;
use std::{collections::BTreeMap, sync::Arc};

/// SQL filtering and grouped aggregation survive logical/physical lowering;
/// rebinding the compiled DAG runs against new data rather than cached results.
#[tokio::test]
async fn sql_filter_grouped_sum_executes_and_rebinds() {
    let catalog = SqlCatalog::new().with_table(
        "metrics",
        Schema::new(vec![
            Field::plain("service", DataType::Utf8, false),
            Field::plain("value", DataType::Float64, true),
        ]),
    );
    for query in [
        "SELECT service, SUM(value) AS total FROM metrics WHERE value > 1 GROUP BY service",
        "SELECT service, SUM(value) AS total FROM metrics GROUP BY service",
    ] {
        let logical = lower_sql(query, &catalog, AccuracyTarget::Exact)
            .await
            .unwrap();
        let space = search_workload(vec![("sql", logical)]);
        let selected = space
            .global_selection(&DefaultCostModel)
            .assemble_selected_dag(&space.roots[0].1)
            .unwrap()
            .unwrap();
        let dag = compile_physical_asap_dag(&selected).unwrap();
        let scan = dag
            .nodes
            .iter()
            .find(|node| {
                matches!(
                    &node.payload,
                    PhysicalASAPOperatorPayload::NonASAP(asap_types::ir::NonASAPOp::Scan { .. })
                )
            })
            .expect("raw SQL scan");
        let schema = Arc::new(scan.output_schema.clone());
        assert!(schema
            .fields
            .iter()
            .all(|field| matches!(field.dtype, FieldDataType::Plain(_))));
        let plan = compile(
            &dag,
            BTreeMap::from([(scan.id as u64, InputContract::bounded(schema.clone()))]),
            &[dag.roots[0] as u64],
        )
        .unwrap();
        for multiplier in [1., 2.] {
            let rows = [
                ("api", Some(2.)),
                ("api", Some(3.)),
                ("api", None),
                ("batch", Some(4.)),
                ("batch", Some(1.)),
            ]
            .into_iter()
            .map(|(service, value)| {
                schema
                    .fields
                    .iter()
                    .map(|field| match field.name.as_str() {
                        "service" => Value::Utf8(service.into()),
                        "value" => {
                            value.map_or(Value::Null, |value| Value::Float64(value * multiplier))
                        }
                        _ => panic!("unexpected field {field:?}"),
                    })
                    .collect()
            })
            .collect();
            let PhysicalASAPOperatorPayload::NonASAP(asap_types::ir::NonASAPOp::Scan {
                source,
                predicates: _,
                schema: _scan_schema,
            }) = &scan.payload
            else {
                unreachable!()
            };
            let expression = asap_types::ir::OperatorNode::reachable(&selected).into_iter().find(|n| matches!(n.non_asap(), Some(asap_types::ir::NonASAPOp::Scan { source: s, .. }) if s == source)).unwrap();
            let mut sources = DataSources::default();
            sources
                .register(
                    source.clone(),
                    Arc::new(
                        MemorySource::new(
                            schema.clone(),
                            vec![Batch::try_new(schema.clone(), rows).unwrap()],
                        )
                        .unwrap(),
                    ),
                )
                .unwrap();
            let bound = plan
                .instantiate(BTreeMap::from([(
                    scan.id as u64,
                    Box::new(sources.bind(&expression).unwrap()) as Source<'_>,
                )]))
                .unwrap();
            let mut stream = bound
                .execute(
                    plan.roots(),
                    RunContext::new(
                        Scope::Query {
                            evaluation_time_ms: 300_000,
                            revision: 1,
                        },
                        Limits::default(),
                    )
                    .unwrap(),
                )
                .unwrap()
                .remove(0);
            let mut batches = Vec::new();
            while let Some(batch) = stream.next().await {
                batches.push(batch.unwrap());
            }
            let mut actual: Vec<_> = batches
                .iter()
                .flat_map(|batch| batch.rows())
                .map(|row| {
                    let Value::Utf8(service) = &row[0] else {
                        panic!("missing service")
                    };
                    let Value::Float64(value) = row[1] else {
                        panic!("missing sum")
                    };
                    (service.to_string(), value)
                })
                .collect();
            actual.sort_by(|a, b| a.0.cmp(&b.0));
            assert_eq!(
                actual,
                vec![
                    ("api".into(), 5. * multiplier),
                    (
                        "batch".into(),
                        4. * multiplier
                            + if query.contains("WHERE") && multiplier == 1. {
                                0.
                            } else {
                                multiplier
                            }
                    )
                ]
            );
        }
    }
}
