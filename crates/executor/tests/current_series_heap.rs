//! Spatial heap weights come from a fresh instant vector, never sample history.
mod common;
use asap_executor::{
    operators::Operator,
    physical_planner::{
        promql_rows::{decode_series_identity, series_row, SERIES_IDENTITY_COLUMN},
        CompiledPhysicalDAG, InputContract, Source,
    },
    runtime::{Limits, RunContext, Scope},
    values::{Batch, Value},
};
use common::compile_physical_asap_dag;
use futures::{executor::block_on, StreamExt};
use planner_types::ir::physical_export::PhysicalASAPOperatorPayload;
use planner_types::ir::schema::DataType;
use planner_types::ir::schema::*;
use std::{collections::BTreeMap, sync::Arc};

fn schema() -> Arc<Schema> {
    Arc::new(planner_types::ir::schema::Schema {
        unique_keys: vec![],
        closed: false,
        fields: [
            ("ts", DataType::Timestamp),
            ("value", DataType::Float64),
            ("job", DataType::Utf8),
            (SERIES_IDENTITY_COLUMN, DataType::Utf8),
        ]
        .into_iter()
        .map(|(name, dtype)| Field {
            table: None,
            name: name.into(),
            dtype: FieldDataType::Plain(dtype),
            nullable: false,
        })
        .collect(),
        time_index: Some(0),
    })
}
fn run(program: &CompiledPhysicalDAG, data: Batch, end: i64) -> Result<Vec<Batch>, String> {
    let recovered = serde_json::from_slice::<CompiledPhysicalDAG>(
        &serde_json::to_vec(&program).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    let input_id = recovered.input_contracts().next().unwrap().0;
    let dag = recovered
        .instantiate(BTreeMap::from([(
            input_id,
            Box::new(Operator::source(data.schema().clone(), vec![data]).unwrap()) as Source<'_>,
        )]))
        .unwrap();
    let context = RunContext::new(
        Scope::Query {
            evaluation_time_ms: end,
            revision: 0,
        },
        Limits::default(),
    )
    .unwrap();
    block_on(async {
        let mut stream = dag.execute(recovered.roots(), context).unwrap().remove(0);
        let mut batches = Vec::new();
        while let Some(batch) = stream.next().await {
            batches.push((*batch.map_err(|e| e.to_string())?).clone());
        }
        Ok(batches)
    })
}
fn input(samples: &[(&str, i64, f64)]) -> Batch {
    let schema = schema();
    let rows = samples
        .iter()
        .map(|(instance, time, value)| {
            series_row(
                &schema,
                &BTreeMap::from([
                    ("job".into(), "api".into()),
                    ("hidden_instance".into(), (*instance).into()),
                ]),
                *time,
                *value,
            )
            .unwrap()
        })
        .collect();
    Batch::try_new(schema, rows).unwrap()
}
fn snapshot_plan() -> CompiledPhysicalDAG {
    CompiledPhysicalDAG::from_operators(
        BTreeMap::from([(0, InputContract::bounded(schema()))]),
        BTreeMap::from([(
            1,
            (
                vec![0],
                Operator::current_series(schema(), 3, 0, 1, 60_000).unwrap(),
            ),
        )]),
        vec![1],
    )
    .unwrap()
}

// Replacement, expiry and stale markers act before sketch updates. Hidden labels
// survive even when every series has the same projected `job` value.
#[test]
fn latest_snapshot_replaces_decreases_expires_and_retains_full_identity() {
    let plan = snapshot_plan();
    let batches = run(
        &plan,
        input(&[
            ("decrease", 10_000, 100.),
            ("decrease", 50_000, 1.),
            ("steady", 40_000, 20.),
            ("expired", 0, 1_000.),
            ("stale", 20_000, 500.),
            ("stale", 55_000, f64::from_bits(0x7ff0_0000_0000_0002)),
            ("future", 60_001, 2_000.),
        ]),
        60_000,
    )
    .unwrap();
    let values = batches
        .iter()
        .flat_map(|batch| batch.rows())
        .map(|row| {
            let Value::Utf8(identity) = &row[3] else {
                panic!()
            };
            let Value::Float64(value) = row[1] else {
                panic!()
            };
            assert!(matches!(row[0], Value::Timestamp(60_000)));
            (
                decode_series_identity(identity).unwrap()["hidden_instance"].clone(),
                value,
            )
        })
        .collect::<BTreeMap<_, _>>();
    assert_eq!(
        values,
        BTreeMap::from([("decrease".into(), 1.), ("steady".into(), 20.)])
    );
    assert!(run(&plan, input(&[("steady", 40_000, 20.)]), 100_000)
        .unwrap()
        .iter()
        .all(|batch| batch.rows().is_empty()));
    assert!(run(
        &plan,
        input(&[("conflict", 50_000, 1.), ("conflict", 50_000, 2.)]),
        60_000
    )
    .is_err());
}

#[test]
fn spatial_heap_ranks_latest_values_in_independent_runs() {
    for algorithm in [
        SketchAlgorithm::CmsWithHeap,
        SketchAlgorithm::CountSketchWithHeap,
    ] {
        let params = match algorithm {
            SketchAlgorithm::CmsWithHeap => SketchParams::CmsWithHeap {
                width: 2048,
                depth: 5,
                heap_size: 100,
            },
            _ => SketchParams::CountSketchWithHeap {
                width: 2048,
                depth: 5,
                heap_size: 100,
            },
        };
        let family = FieldDataType::Sketch(SketchKind::new(algorithm, params), Default::default());
        let build = Operator::keyed_summary_build(schema(), family, 1, vec![3], vec![2]).unwrap();
        let output = Arc::new(planner_types::ir::schema::Schema {
            unique_keys: vec![],
            closed: false,
            fields: vec![
                schema().fields[2].clone(),
                schema().fields[3].clone(),
                schema().fields[1].clone(),
            ],
            time_index: None,
        });
        let read = Operator::keyed_evaluation(build.schema(), 1, 1, output).unwrap();
        let plan = CompiledPhysicalDAG::from_operators(
            BTreeMap::from([(0, InputContract::bounded(schema()))]),
            BTreeMap::from([
                (
                    1,
                    (
                        vec![0],
                        Operator::current_series(schema(), 3, 0, 1, 60_000).unwrap(),
                    ),
                ),
                (2, (vec![1], build)),
                (3, (vec![2], read)),
            ]),
            vec![3],
        )
        .unwrap();
        for (samples, end, winner, score) in [
            (
                vec![("a", 10_000, 100.), ("a", 50_000, 1.), ("b", 50_000, 20.)],
                60_000,
                "b",
                20.,
            ),
            (
                vec![("a", 110_000, 3.), ("b", 50_000, 20.)],
                120_000,
                "a",
                3.,
            ),
        ] {
            let batches = run(&plan, input(&samples), end).unwrap();
            let rows = batches
                .iter()
                .flat_map(|batch| batch.rows())
                .collect::<Vec<_>>();
            assert_eq!(rows.len(), 1);
            let Value::Utf8(encoded) = &rows[0][1] else {
                panic!()
            };
            assert_eq!(
                decode_series_identity(encoded).unwrap()["hidden_instance"],
                winner
            );
            assert!(matches!(rows[0][2], Value::Float64(actual) if actual == score));
        }
    }
}

// Blocking membership selection shares the run's cancellation and byte budget.
#[test]
fn current_series_observes_resource_limits() {
    use asap_executor::Error;
    let plan = snapshot_plan();
    for cancelled in [false, true] {
        let data = input(&[("one", 50_000, 1.)]);
        let dag = plan
            .instantiate(BTreeMap::from([(
                0,
                Box::new(Operator::source(data.schema().clone(), vec![data]).unwrap())
                    as Source<'_>,
            )]))
            .unwrap();
        let context = RunContext::new(
            Scope::Query {
                evaluation_time_ms: 60_000,
                revision: 0,
            },
            Limits {
                max_bytes: if cancelled { 1 << 20 } else { 1 },
                ..Limits::default()
            },
        )
        .unwrap();
        if cancelled {
            context.cancel();
        }
        let result = match dag.execute(&[1], context.clone()) {
            Err(error) => Err(error),
            Ok(mut streams) => block_on(streams.remove(0).next()).unwrap().map(|_| ()),
        };
        assert!(matches!(
            (cancelled, result),
            (true, Err(Error::Cancelled)) | (false, Err(Error::MemoryLimit))
        ));
        assert_eq!(context.retained_bytes(), 0);
    }
}

#[test]
fn identity_encoding_is_lossless_and_rejects_noncanonical_inputs() {
    use asap_executor::physical_planner::promql_rows::encode_series_identity;
    let labels = BTreeMap::from([
        ("a".into(), "quote\"slash\\".into()),
        ("other".into(), "".into()),
    ]);
    assert_eq!(
        decode_series_identity(&encode_series_identity(&labels).unwrap()).unwrap(),
        labels
    );
    for invalid in [
        "[]",
        "{\"a\":1}",
        "{\"a\":\"x\",\"a\":\"x\"}",
        "{ \"a\":\"x\"}",
    ] {
        assert!(decode_series_identity(invalid).is_err(), "{invalid}");
    }
}

// The actual Planner population candidate lowers to native operators; this
// test does not manually assemble the computation or its dependency edges.
#[test]
fn planner_current_series_candidate_compiles_with_dynamic_identity() {
    use asap_executor::physical_planner::{compile, promql_rows::with_series_identity};
    use planner_types::{types::AccuracyTarget, workload::*};
    use std::rc::Rc;
    let workload = PlanningWorkload {
        query_workload: QueryWorkload {
            language: QueryLanguage::PromQL,
            query_batch: Some(vec![BatchEntry {
                query: Query("topk by(job)(1, m)".into()),
                requirements: QueryRequirements {
                    accuracy: AccuracyRequirement::Explicit(AccuracyTarget::Exact),
                    ..Default::default()
                },
                predictability: Predictability::Unknown,
                invocations: 1,
                execute_at: None,
                time_selection: TimeSelection::default(),
            }]),
            repeating_queries: None,
        },
        data_workload: Some(DataWorkload {
            data_ingestion_interval: Evidence {
                value: Some(DurationMs(60_000)),
                ..Default::default()
            },
            ..Default::default()
        }),
    };
    let original = asap_frontend_promql::lower_promql_workload(&workload, 0)
        .unwrap()
        .remove(0);
    let open_root = Rc::new(original.clone());
    let open_selected =
        asap_logical_optimizer::pass1::maintained_population::MaintainedPopulationStrategy::new(
            std::slice::from_ref(&open_root),
        )
        .candidate(&open_root)
        .unwrap();
    let snapshot_program =
        asap_executor::physical_planner::promql_rows::compile_current_series_evaluation(
            &open_selected,
        )
        .unwrap();
    let encoded = String::from_utf8(serde_json::to_vec(&snapshot_program).unwrap()).unwrap();
    assert!(
        !encoded.contains("CurrentSeries"),
        "maintained input must not be rebuilt"
    );
    assert!(encoded.contains("Sort") && encoded.contains("Limit"));
    assert_eq!(snapshot_program.input_contracts().count(), 1);
    let root = Rc::new(with_series_identity(&original).unwrap());
    let selected =
        asap_logical_optimizer::pass1::maintained_population::MaintainedPopulationStrategy::new(
            std::slice::from_ref(&root),
        )
        .candidate(&root)
        .unwrap();
    let logical = compile_physical_asap_dag(&selected).unwrap();
    let raw = logical
        .nodes
        .iter()
        .find(|node| {
            matches!(
                node.payload,
                PhysicalASAPOperatorPayload::NonASAP(
                    planner_types::ir::NonASAPOp::TimeRange { .. }
                )
            )
        })
        .unwrap();
    let raw_schema = Arc::new(raw.output_schema.clone());
    let physical = compile(
        &logical,
        BTreeMap::from([(raw.id as u64, InputContract::bounded(raw_schema.clone()))]),
        &[logical.roots[0] as u64],
    )
    .unwrap();
    let bytes = String::from_utf8(serde_json::to_vec(&physical).unwrap()).unwrap();
    assert!(bytes.contains("CurrentSeries"));
    assert!(bytes.contains("Sort"));
    assert!(bytes.contains("Limit"));
    let rows = [("a", 10_000, 100.), ("a", 50_000, 1.), ("b", 50_000, 20.)]
        .into_iter()
        .map(|(member, at, value)| {
            series_row(
                &raw_schema,
                &BTreeMap::from([
                    ("job".into(), "api".into()),
                    ("unreferenced".into(), member.into()),
                ]),
                at,
                value,
            )
            .unwrap()
        })
        .collect();
    let batches = run(&physical, Batch::try_new(raw_schema, rows).unwrap(), 60_000).unwrap();
    let rows = batches
        .iter()
        .flat_map(|batch| batch.rows())
        .collect::<Vec<_>>();
    assert_eq!(rows.len(), 1);
    assert!(matches!(rows[0][1], Value::Float64(20.)));
    let id = batches[0]
        .schema()
        .fields
        .iter()
        .position(|field| field.name == SERIES_IDENTITY_COLUMN)
        .unwrap();
    let Value::Utf8(encoded) = &rows[0][id] else {
        panic!()
    };
    assert_eq!(
        decode_series_identity(encoded).unwrap()["unreferenced"],
        "b"
    );
}
