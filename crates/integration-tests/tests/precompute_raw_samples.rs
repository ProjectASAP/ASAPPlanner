//! Planner-selected summaries over raw data samples compile, bound to raw data
//! at their `Scan`, and produce the same estimates as feeding their kernel
//! sample by sample.
use crate::physical_common;
use asap_types::ir::physical_export::{PhysicalASAPDAG, PhysicalASAPOperatorPayload};
use asap_types::ir::ASAPOp;
use asap_types::ir::OperatorNode;
use physical_common::compile_at_ingestion_physical_asap_dag;
use std::{collections::BTreeMap, collections::BTreeSet, rc::Rc, sync::Arc};

use asap_executor::{
    factory::create_planner_accumulator,
    physical_planner::{compile, promql_rows, CompiledPhysicalDAG},
    runtime::{Limits, RunContext, Scope},
    summary_kernels::{exact::ExactAccumulator, weighted_frequency::WeightedFrequency},
    values::Value,
    AggregateCore,
};
use asap_integration_tests::fixtures::lower_promql;
use asap_integration_tests::planning::explicit_local_candidates;
use asap_types::ir::operator::Reduction;
use asap_types::ir::properties::AccuracyTarget;
use asap_types::ir::scalar::expr_ir::ColumnRef;
use asap_types::ir::schema::{
    FieldDataType, SketchAlgorithm, SketchStatistic, SummaryInputExpr, SummaryUpdate,
};
use futures::{executor::block_on, StreamExt};

type Series = BTreeMap<String, String>;

/// A series label set; `None` omits the label. An empty value is present
/// in the input but is not part of the series identity.
fn series(service: Option<&str>, instance: &str) -> Series {
    [
        ("__name__", Some("m")),
        ("service", service),
        ("instance", Some(instance)),
    ]
    .into_iter()
    .filter_map(|(k, v)| Some((k.to_owned(), v?.to_owned())))
    .collect()
}

fn canonical(labels: &Series) -> Series {
    labels
        .iter()
        .filter(|(_, v)| !v.is_empty())
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

/// Exact and explicit single-rule candidates for `query`, with
/// `m` declared a counter: plan selection's answer to a heap's open
/// requirement that its weights are counter samples, which the executor
/// needs before it builds a Count-Min heap.
fn candidates(query: &str, accuracy: AccuracyTarget) -> Vec<Rc<OperatorNode>> {
    use asap_types::ir::QueryRoot;
    use asap_types::workload::bounds::with_confirmed_weights;
    use asap_types::workload::{DataWorkload, MetricType, SourceFacts};
    let root = lower_promql(query, accuracy.clone()).expect("lowering failed");
    let root = promql_rows::with_series_identity(&root).unwrap();
    let counter = SourceFacts {
        metric_type: Some(MetricType::Counter),
        ..SourceFacts::default()
    };
    let data = DataWorkload {
        sources: [("m".to_string(), counter)].into(),
        ..Default::default()
    };
    explicit_local_candidates(root)
        .into_iter()
        .map(|candidate| {
            match &with_confirmed_weights(&[QueryRoot::Operator(candidate)], &data)[..] {
                [QueryRoot::Operator(confirmed)] => Rc::clone(confirmed),
                _ => unreachable!("one operator root"),
            }
        })
        .collect()
}

/// Raw-input summary nodes: `(dag, raw source id, summary id)`.
fn raw_summaries(dag: &PhysicalASAPDAG) -> Vec<(u64, u64)> {
    dag.nodes
        .iter()
        .filter(|node| {
            matches!(
                node.payload,
                PhysicalASAPOperatorPayload::ASAP(ASAPOp::SummaryAgg { .. })
            )
        })
        .filter_map(|node| {
            let inputs = dag
                .edges
                .iter()
                .filter(|edge| edge.consumer == node.id)
                .collect::<Vec<_>>();
            let [edge] = inputs.as_slice() else {
                return None;
            };
            let source = dag.nodes.iter().find(|n| n.id == edge.producer)?;
            matches!(
                source.payload,
                PhysicalASAPOperatorPayload::NonASAP(asap_types::ir::NonASAPOp::TimeRange { .. })
            )
            .then_some((source.id as u64, node.id as u64))
        })
        .collect()
}

fn samples() -> Vec<(Series, i64, f64)> {
    let mut rows = Vec::new();
    let series_set = [
        (Some("a"), "1"),
        (Some("a"), "2"),
        (Some("b"), "1"),
        (None, "3"),
        (Some("b"), ""),
    ];
    for (index, (service, instance)) in series_set.iter().enumerate() {
        for step in 1..=5i64 {
            let value = (index as f64 + 1.0) * step as f64 + (step % 2) as f64;
            rows.push((series(*service, instance), step * 1000, value));
        }
    }
    rows
}

/// Compile the summary `root` alone, its raw data read at the `Scan`.
fn compile_summary(dag: &PhysicalASAPDAG, root: u64) -> Result<CompiledPhysicalDAG, String> {
    compile(dag, &[], &[root]).map_err(|error| error.to_string())
}

/// Run the summary `root` in one ingestion run over `rows`, bound as raw
/// data at its `Scan`: each population's labels and state.
fn execute(
    dag: &PhysicalASAPDAG,
    root: u64,
    rows: &[(Series, i64, f64)],
) -> Vec<(Series, Arc<dyn AggregateCore>)> {
    let program = compile_summary(dag, root).unwrap_or_else(|error| {
        panic!("raw summary {root} does not compile: {error}");
    });
    let store = physical_common::raw_data_store(dag, |_, schema| {
        rows.iter()
            .map(|(labels, time, value)| {
                promql_rows::series_row(schema, &canonical(labels), *time, *value).unwrap()
            })
            .collect()
    });
    let output = program.output_schema(root).unwrap();
    let physical_dag = program.instantiate(Arc::new(store)).unwrap();
    let context = RunContext::new(
        Scope::Ingestion {
            window_start_ms: 0,
            window_end_ms: 6000,
        },
        Limits::default(),
    )
    .unwrap();
    block_on(async {
        let mut stream = physical_dag
            .execute(program.roots(), context)
            .unwrap()
            .remove(0);
        let mut result = Vec::new();
        while let Some(batch) = stream.next().await {
            for row in &batch.unwrap().to_rows() {
                let mut labels = Series::new();
                let mut state = None;
                for (field, value) in output.fields.iter().zip(row) {
                    match value {
                        Value::Utf8(identity)
                            if field.name == promql_rows::SERIES_IDENTITY_COLUMN =>
                        {
                            labels.extend(promql_rows::decode_series_identity(identity).unwrap())
                        }
                        Value::Utf8(label) => {
                            labels.insert(field.name.clone(), label.to_string());
                        }
                        Value::Summary { state: s, .. } => state = Some(s.clone()),
                        Value::Timestamp(6000) => {}
                        other => panic!("unexpected population column {other:?} in {row:?}"),
                    }
                }
                result.push((canonical(&labels), state.expect("a summary state")));
            }
        }
        result
    })
}

/// Each series' latest sample, moved to the ingestion run's end (6000 ms).
fn latest_per_series(rows: &[(Series, i64, f64)]) -> Vec<(Series, i64, f64)> {
    let mut latest = BTreeMap::<Series, (i64, f64)>::new();
    for (labels, time, value) in rows {
        let entry = latest.entry(canonical(labels)).or_insert((*time, *value));
        if *time >= entry.0 {
            *entry = (*time, *value);
        }
    }
    latest
        .into_iter()
        .map(|(labels, (_, value))| (labels, 6000, value))
        .collect()
}

/// PromQL grouping of a canonical label set: `by` keeps the named labels,
/// `without` drops them and `__name__`.
fn population(reduction: &Reduction, dag_labels: &[String], labels: &Series) -> Series {
    let labels = canonical(labels);
    match reduction {
        Reduction::PerEntity => labels,
        Reduction::Reduce(keys) => labels
            .into_iter()
            .filter(|(k, _)| {
                if keys.is_without() {
                    k != "__name__" && !dag_labels.contains(k)
                } else {
                    dag_labels.contains(k)
                }
            })
            .collect(),
    }
}

/// Item identity used by a keyed summary: labels by name, the sample value,
/// or the canonical label-set identity.
fn item(expr: &SummaryInputExpr, labels: &Series, value: f64, out: &mut Vec<Value>) {
    match expr {
        SummaryInputExpr::Column(ColumnRef::SampleValue) => out.push(Value::Float64(value)),
        SummaryInputExpr::Column(ColumnRef::Named(name)) if name == "value" => {
            out.push(Value::Float64(value))
        }
        SummaryInputExpr::Column(ColumnRef::Named(name))
            if name == promql_rows::SERIES_IDENTITY_COLUMN =>
        {
            out.push(Value::Utf8(
                promql_rows::encode_series_identity(&canonical(labels))
                    .unwrap()
                    .into(),
            ))
        }
        SummaryInputExpr::Column(ColumnRef::Named(name)) => out.push(Value::Utf8(
            canonical(labels)
                .get(name)
                .cloned()
                .unwrap_or_default()
                .into(),
        )),
        SummaryInputExpr::Tuple(items) => items.iter().for_each(|i| item(i, labels, value, out)),
        other => panic!("unsupported fixture item {other:?}"),
    }
}

fn weight(update: &SummaryUpdate, value: f64) -> f64 {
    match update.weight {
        SummaryInputExpr::Constant(weight) => weight,
        _ => value,
    }
}

/// Estimates that identify a state's content for comparison.
fn evaluations(state: &dyn AggregateCore, family: &FieldDataType) -> Vec<f64> {
    if let Some(exact) = state.as_any().downcast_ref::<ExactAccumulator>() {
        let FieldDataType::ExactAggregate(kind, _) = family else {
            unreachable!()
        };
        return vec![exact.evaluation(*kind, None, None).unwrap().unwrap()];
    }
    let FieldDataType::Sketch(kind, _) = family else {
        panic!("sketch state for exact family")
    };
    match kind.algorithm() {
        SketchAlgorithm::Kll | SketchAlgorithm::DDSketch => [0.1, 0.5, 0.9]
            .into_iter()
            .map(|q| state.estimate(&SketchStatistic::Quantile { q }).unwrap())
            .collect(),
        SketchAlgorithm::Hll | SketchAlgorithm::Kmv => {
            vec![state.estimate(&SketchStatistic::Cardinality).unwrap()]
        }
        SketchAlgorithm::UnivMon => [
            SketchStatistic::Cardinality,
            SketchStatistic::FrequencyL2,
            SketchStatistic::FrequencyEntropy,
        ]
        .iter()
        .map(|statistic| state.estimate(statistic).unwrap())
        .collect(),
        other => panic!("unexpected unkeyed sketch {other:?}"),
    }
}

/// Compile one raw-input summary, execute it over `rows`, and compare each
/// population with its kernel fed sample by sample. Returns the family label,
/// or the family when it has no native state.
fn check(
    query: &str,
    dag: &PhysicalASAPDAG,
    source: u64,
    root: u64,
    rows: &[(Series, i64, f64)],
) -> Result<String, String> {
    let node = dag.nodes.iter().find(|n| n.id as u64 == root).unwrap();
    let PhysicalASAPOperatorPayload::ASAP(ASAPOp::SummaryAgg {
        family,
        input,
        reduction,
        grouping,
        ..
    }) = &node.payload
    else {
        unreachable!()
    };
    let source_node = dag.nodes.iter().find(|n| n.id as u64 == source).unwrap();
    // An instant selector keeps each series' latest sample within its
    // lookback before the run's end: feed one such sample per series.
    let latest_rows;
    let rows = if matches!(
        source_node.payload,
        PhysicalASAPOperatorPayload::NonASAP(asap_types::ir::NonASAPOp::TimeRange {
            kind: asap_types::ir::TimeRangeKind::Instant,
            ..
        })
    ) {
        latest_rows = latest_per_series(rows);
        &latest_rows[..]
    } else {
        rows
    };
    let keys = match reduction {
        Reduction::Reduce(keys) => keys
            .keys()
            .iter()
            .map(|i| source_node.output_schema.fields[*i].name.clone())
            .collect(),
        Reduction::PerEntity => vec![],
    };
    let stored_only = matches!(family, FieldDataType::Sketch(kind, _)
        if kind.algorithm() == &asap_types::ir::schema::SketchAlgorithm::Cms);
    if stored_only || asap_executor::capability::validate_native_family(family).is_err() {
        // Families without a native state, or with native stored state only (plain CMS), are outside raw summary execution;
        // their compile must fail.
        assert!(compile_summary(dag, root).is_err());
        return Err(format!("{family:?}"));
    }
    let actual = execute(dag, root, rows);
    let label = match family {
        FieldDataType::ExactAggregate(kind, _) => format!("{kind:?}"),
        FieldDataType::Sketch(kind, _) => format!("{:?}", kind.algorithm()),
        other => format!("{other:?}"),
    };
    if let FieldDataType::Sketch(kind, _) = family {
        if let (Some(keyed), false) = (
            &input.item,
            matches!(
                kind.algorithm(),
                SketchAlgorithm::Hll | SketchAlgorithm::Kmv | SketchAlgorithm::UnivMon
            ),
        ) {
            // Keyed heaps: every item's estimated weight is its exact
            // total at this scale (no collisions in the fixture).
            let mut expected = BTreeMap::<Series, BTreeMap<String, f64>>::new();
            for (labels, _, value) in rows {
                let mut items = Vec::new();
                item(keyed, labels, *value, &mut items);
                *expected
                    .entry(population(reduction, &keys, labels))
                    .or_default()
                    .entry(format!("{items:?}"))
                    .or_default() += weight(input, *value);
            }
            assert_eq!(actual.len(), expected.len(), "{query}");
            for (labels, state) in &actual {
                let heap = state.as_any().downcast_ref::<WeightedFrequency>().unwrap();
                let got = heap
                    .rows(usize::MAX >> 1)
                    .into_iter()
                    .map(|mut row| {
                        let Some(Value::Float64(score)) = row.pop() else {
                            panic!("heap score")
                        };
                        (format!("{row:?}"), score)
                    })
                    .collect::<BTreeMap<_, _>>();
                assert_eq!(&got, &expected[labels], "{query}");
            }
            return Ok(label);
        }
    }
    let mut expected =
        BTreeMap::<Series, Box<dyn asap_executor::factory::AccumulatorUpdater>>::new();
    for (labels, time, value) in rows {
        let updater = expected
            .entry(population(reduction, &keys, labels))
            .or_insert_with(|| create_planner_accumulator(family, input, grouping).unwrap());
        let unit = input.item.is_some();
        updater
            .update_single(if unit { *value } else { weight(input, *value) }, *time)
            .unwrap();
    }
    assert_eq!(actual.len(), expected.len(), "{query}");
    for (labels, state) in actual {
        let reference = expected[&labels].snapshot_accumulator();
        assert_eq!(
            evaluations(state.as_ref(), family),
            evaluations(reference.as_ref(), family),
            "{query}: {labels:?}"
        );
    }
    Ok(label)
}

// Every raw-input summary Planner composes compiles over raw data sample rows,
// and each population's estimates equal feeding its kernel sample by sample.
#[test]
fn raw_data_sample_summaries_compile_and_match_their_kernels() {
    let exact = AccuracyTarget::exact();
    let sketch = AccuracyTarget::max_error(0.02);
    // A UnivMon sized for a distinct count at ε = 0.02 holds a heap of
    // millions of keys on each of its 19 layers, past the default 64 MiB.
    let distinct = AccuracyTarget::max_error(0.2).failure_probability(0.1);
    let queries = [
        ("sum_over_time(m[5m])", &exact),
        ("count_over_time(m[5m])", &exact),
        ("min_over_time(m[5m])", &exact),
        ("max_over_time(m[5m])", &exact),
        ("rate(m[5m])", &exact),
        ("increase(m[5m])", &exact),
        ("sum by (service) (sum_over_time(m[5m]))", &exact),
        ("sum by (service) (rate(m[5m]))", &exact),
        ("topk(2, sum_over_time(m[5m]))", &exact),
        ("quantile_over_time(0.9, m[5m])", &sketch),
        ("sum by (service) (quantile_over_time(0.9, m[5m]))", &sketch),
        ("quantile by (service) (0.9, m)", &sketch),
        ("distinct_over_time(m[5m])", &distinct),
        ("count(m)", &sketch),
        ("topk(2, m)", &sketch),
        (
            "topk(2, sum by (service) (count_over_time(m[5m])))",
            &sketch,
        ),
        ("topk(2, sum_over_time(m[5m]))", &sketch),
        ("topk by (service) (2, sum_over_time(m[5m]))", &sketch),
    ];
    let rows = samples();
    let mut families = BTreeSet::new();
    let mut unsupported = BTreeSet::new();
    let mut checked = BTreeMap::new();
    for (query, accuracy) in queries {
        for candidate in candidates(query, accuracy.clone()) {
            let dag = compile_at_ingestion_physical_asap_dag(&candidate).unwrap();
            for (source, root) in raw_summaries(&dag) {
                match check(query, &dag, source, root, &rows) {
                    Ok(family) => {
                        families.insert(family);
                        *checked.entry(query).or_insert(0) += 1;
                    }
                    Err(family) => {
                        unsupported.insert(family);
                    }
                }
            }
        }
    }
    println!("checked {checked:?}; families {families:?}; without native state {unsupported:?}");
    for query in [
        "topk by (service) (2, sum_over_time(m[5m]))",
        "quantile by (service) (0.9, m)",
        "distinct_over_time(m[5m])",
    ] {
        assert!(
            checked.contains_key(query),
            "{query} has no checked raw summary"
        );
    }
    for family in [
        "Sum",
        "Count",
        "Min",
        "Max",
        "Rate",
        "Increase",
        "Kll",
        "DDSketch",
        "Hll",
        "Kmv",
        "UnivMon",
        // The whole-expression heaps over raw data samples.
        "CmsWithHeap",
        "CountSketchWithHeap",
    ] {
        assert!(
            families.contains(family),
            "no {family} fixture: {families:?}"
        );
    }
}

/// Replace the raw summary of `sum by (service) (sum_over_time(m{instance!="none"}[5m]))` with
/// another update, keeping its raw input and reduction.
fn grouped_raw_summary(family: FieldDataType, input: SummaryUpdate) -> (PhysicalASAPDAG, u64, u64) {
    // The matcher, true of every sample, makes the `Scan` read `instance`.
    let candidate = candidates(
        r#"sum by (service) (sum_over_time(m{instance!="none"}[5m]))"#,
        AccuracyTarget::exact(),
    )
    .pop()
    .unwrap();
    let mut dag = compile_at_ingestion_physical_asap_dag(&candidate).unwrap();
    let (source, root) = raw_summaries(&dag)[0];
    let node = dag.nodes.iter_mut().find(|n| n.id as u64 == root).unwrap();
    let PhysicalASAPOperatorPayload::ASAP(ASAPOp::SummaryAgg {
        family: old,
        input: update,
        ..
    }) = &mut node.payload
    else {
        unreachable!()
    };
    for field in &mut node.output_schema.fields {
        if field.dtype == *old {
            field.dtype = family.clone();
        }
    }
    *old = family;
    *update = input;
    let schema = node.output_schema.clone();
    for edge in dag.edges.iter_mut().filter(|e| e.producer as u64 == root) {
        *edge.intermediate_schema_mut() = schema.clone();
    }
    (dag, source, root)
}

// Keyed heaps over raw data samples resolve items from the series label set and
// estimate each item's exact total; invalid weight contracts do not compile.
#[test]
fn raw_data_sample_heaps_resolve_items_from_labels() {
    use asap_types::ir::schema::GroupingStrategy::PerSubpopulationInstance;
    use asap_types::ir::schema::{NonNegativeWeightProof, SketchKind, SketchParams, WeightDomain};
    let heap = |algorithm, params| {
        FieldDataType::Sketch(SketchKind::new(algorithm, params), PerSubpopulationInstance)
    };
    let cms = heap(
        SketchAlgorithm::CmsWithHeap,
        SketchParams::CmsWithHeap {
            width: 64,
            depth: 3,
            heap_size: 8,
        },
    );
    let count_sketch = heap(
        SketchAlgorithm::CountSketchWithHeap,
        SketchParams::CountSketchWithHeap {
            width: 64,
            depth: 3,
            heap_size: 8,
        },
    );
    // The series label set, read from the `Scan`'s series identity column.
    let identity = SummaryUpdate {
        item: Some(SummaryInputExpr::Column(ColumnRef::Named(
            promql_rows::SERIES_IDENTITY_COLUMN.into(),
        ))),
        weight: SummaryInputExpr::Constant(1.0),
        weight_domain: WeightDomain::NonNegative {
            proof: NonNegativeWeightProof::UnitCount,
        },
    };
    let by_instance = SummaryUpdate {
        item: Some(SummaryInputExpr::Tuple(vec![SummaryInputExpr::Column(
            ColumnRef::Named("instance".into()),
        )])),
        weight: SummaryInputExpr::Column(ColumnRef::SampleValue),
        weight_domain: WeightDomain::UnknownOrSigned,
    };
    let rows = samples();
    for (family, input) in [
        (cms.clone(), identity.clone()),
        (count_sketch.clone(), identity),
        (count_sketch, by_instance.clone()),
    ] {
        let unit_count = input.weight == SummaryInputExpr::Constant(1.0);
        let (dag, source, root) = grouped_raw_summary(family, input);
        assert!(check("heap", &dag, source, root, &rows).is_ok());
        // A unit count adds the constant 1 per row; no column of ones is
        // projected for it.
        let compiled = serde_json::to_value(compile_summary(&dag, root).unwrap()).unwrap();
        assert_eq!(
            compiled
                .to_string()
                .contains(r#""row_weight":{"Constant":1}"#),
            unit_count,
            "{compiled}"
        );
    }
    let signed_cms = grouped_raw_summary(cms, by_instance.clone());
    assert!(compile_summary(&signed_cms.0, signed_cms.2).is_err());
}

// End to end: `topk(2, count by (service) (m))` at ε = 0.02 has a Count-Min
// heap candidate whose update is the unit count (`Constant(1.0)`). Placed at
// ingestion, its raw-data-sample precompute adds the constant 1 per sample to each
// service's count, with no column of ones projected, and each heap reads the
// exact per-service sample counts of the fixture.
#[test]
fn count_ranked_heap_precompute_adds_a_constant_row_weight() {
    let query = "topk(2, count by (service) (m))";
    let rows = samples();
    let mut checked = 0;
    for candidate in candidates(query, AccuracyTarget::max_error(0.02)) {
        let dag = compile_at_ingestion_physical_asap_dag(&candidate).unwrap();
        for (source, root) in raw_summaries(&dag) {
            let node = dag.nodes.iter().find(|n| n.id as u64 == root).unwrap();
            let PhysicalASAPOperatorPayload::ASAP(ASAPOp::SummaryAgg {
                family: FieldDataType::Sketch(kind, _),
                input,
                ..
            }) = &node.payload
            else {
                continue;
            };
            if kind.algorithm() != &SketchAlgorithm::CmsWithHeap {
                continue;
            }
            assert_eq!(input.weight, SummaryInputExpr::Constant(1.0));
            let compiled = serde_json::to_value(compile_summary(&dag, root).unwrap())
                .unwrap()
                .to_string();
            assert!(
                compiled.contains(r#""row_weight":{"Constant":1}"#),
                "{compiled}"
            );
            assert_eq!(
                check(query, &dag, source, root, &rows),
                Ok("CmsWithHeap".into())
            );
            checked += 1;
        }
    }
    assert!(checked > 0, "{query} has no Count-Min heap candidate");
}
