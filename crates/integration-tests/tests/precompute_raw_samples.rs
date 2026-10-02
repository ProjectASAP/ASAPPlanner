//! Planner-selected summaries over raw samples compile as precompute DAGs
//! and produce the same estimates as feeding their kernel sample by sample.
use std::{collections::BTreeMap, collections::BTreeSet, rc::Rc, sync::Arc};

use asap_aware_mapping::cost_model::DefaultCostModel;
use asap_aware_mapping::{
    search_workload, Replacement, ReplacementStrategy, ReplacementSubDAG, SketchAlgorithmStrategy,
    TargetSubDAG,
};
use asap_integration_tests::fixtures::lower_promql;
use asap_physical_operators::{
    factory::create_planner_accumulator,
    operators::Operator,
    physical_planner::{precompute, Source},
    runtime::{Limits, RunContext, Scope},
    summary_kernels::{exact::ExactAccumulator, weighted_frequency::WeightedFrequency},
    values::{Batch, Value},
    AggregateCore, KeyByLabelValues, Statistic,
};
use asap_types::post_asap::{
    compile_post_asap_dag, EntityIdentity, ExactKind, FieldDataType, PostAsapDAG,
    PostAsapOperatorPayload, SketchAlgorithm, SketchQuery, SummaryInputExpr, SummaryNode,
    SummaryUpdate,
};
use asap_types::pre_asap::{expr_ir::ColumnRef, query_expr::Reduction};
use asap_types::types::AccuracyTarget;
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

/// Every Planner candidate for `query`: the searched selection plus each
/// summary replacement of the root.
fn candidates(query: &str, accuracy: AccuracyTarget) -> Vec<Rc<SummaryNode>> {
    let root = Rc::new(lower_promql(query, accuracy).expect("lowering failed"));
    let mut result = SketchAlgorithmStrategy::default_cost_model()
        .replacements(&TargetSubDAG::new(&root))
        .into_iter()
        .filter_map(|candidate| match candidate {
            ReplacementSubDAG {
                replacement: Replacement::Summary(node),
                ..
            } => Some(node),
            _ => None,
        })
        .collect::<Vec<_>>();
    let space = search_workload(vec![("query", root)]);
    if let Ok(Some(selected)) = space
        .global_selection(&DefaultCostModel)
        .assemble_selected_dag(&space.roots[0].1)
    {
        result.push(selected);
    }
    result
}

/// Raw-input summary nodes: `(dag, raw source id, summary id)`.
fn raw_summaries(dag: &PostAsapDAG) -> Vec<(u64, u64)> {
    dag.nodes
        .iter()
        .filter(|node| matches!(node.payload, PostAsapOperatorPayload::SummaryAgg { .. }))
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
            matches!(source.payload, PostAsapOperatorPayload::Fallback { .. })
                .then_some((u64::from(source.id.0), u64::from(node.id.0)))
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

fn execute(
    dag: &PostAsapDAG,
    source: u64,
    root: u64,
    rows: &[(Series, i64, f64)],
) -> Vec<(Series, Arc<dyn AggregateCore>)> {
    let program = precompute::compile(dag, &[source], &[root]).unwrap_or_else(|error| {
        panic!(
            "raw summary {root} does not compile: {error}; source {:?}",
            dag.nodes
                .iter()
                .find(|n| u64::from(n.id.0) == source)
                .map(|n| (&n.output_schema, &n.payload))
        );
    });
    let program = serde_json::from_slice::<
        asap_physical_operators::physical_planner::CompiledPhysicalDAG,
    >(&serde_json::to_vec(&program).unwrap())
    .unwrap();
    let schema = precompute::raw_sample_schema();
    let batch = Batch::try_new(
        schema.clone(),
        rows.iter()
            .map(|(labels, time, value)| precompute::raw_sample_row(labels, *time, *value))
            .collect(),
    )
    .unwrap();
    let sources = BTreeMap::from([(
        source,
        Box::new(Operator::source(schema, vec![batch]).unwrap()) as Source<'_>,
    )]);
    let physical_dag = program.instantiate(sources).unwrap();
    let context = RunContext::new(
        Scope::Ingestion {
            window_start_ms: 0,
            window_end_ms: 6000,
            revision: 1,
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
            for row in batch.unwrap().rows() {
                let [Value::Map(labels), Value::Timestamp(6000), Value::Summary { state, .. }] =
                    row.as_slice()
                else {
                    panic!("unexpected population row {row:?}");
                };
                let labels = labels
                    .iter()
                    .map(|(k, v)| match (k, v) {
                        (Value::Utf8(k), Value::Utf8(v)) => (k.to_string(), v.to_string()),
                        _ => panic!("non-label population entry"),
                    })
                    .collect();
                result.push((labels, state.clone()));
            }
        }
        result
    })
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
        SummaryInputExpr::Column(ColumnRef::Named(name)) => out.push(Value::Utf8(
            canonical(labels)
                .get(name)
                .cloned()
                .unwrap_or_default()
                .into(),
        )),
        SummaryInputExpr::EntityIdentity(EntityIdentity::PromqlLabelSet { excluding }) => {
            let mut identity = canonical(labels);
            for column in excluding {
                let ColumnRef::Named(name) = column else {
                    panic!("unsupported fixture exclusion {column:?}")
                };
                identity.remove(name);
            }
            out.push(Value::Utf8(
                serde_json::to_string(&identity).unwrap().into(),
            ))
        }
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
fn readouts(state: &dyn AggregateCore, family: &FieldDataType) -> Vec<f64> {
    if let Some(exact) = state.as_any().downcast_ref::<ExactAccumulator>() {
        let FieldDataType::ExactAggregate(kind, _) = family else {
            unreachable!()
        };
        let statistic = match kind {
            ExactKind::Sum => Statistic::Sum,
            ExactKind::Count => Statistic::Count,
            ExactKind::Min => Statistic::Min,
            ExactKind::Max => Statistic::Max,
            ExactKind::Rate => Statistic::Rate,
            ExactKind::Increase => Statistic::Increase,
            other => panic!("unexpected exact kind {other:?}"),
        };
        return vec![exact
            .readout(statistic, None, None::<&KeyByLabelValues>)
            .unwrap()
            .unwrap()];
    }
    let FieldDataType::Sketch(kind, _) = family else {
        panic!("sketch state for exact family")
    };
    match kind.algorithm() {
        SketchAlgorithm::Kll | SketchAlgorithm::DDSketch => [0.1, 0.5, 0.9]
            .into_iter()
            .map(|q| state.estimate(&SketchQuery::Quantile { q }).unwrap())
            .collect(),
        SketchAlgorithm::Hll => vec![state.estimate(&SketchQuery::Cardinality).unwrap()],
        other => panic!("unexpected unkeyed sketch {other:?}"),
    }
}

/// Compile one raw-input summary, execute it over `rows`, and compare each
/// population with its kernel fed sample by sample. Returns the family label,
/// or the family when it has no native state.
fn check(
    query: &str,
    dag: &PostAsapDAG,
    source: u64,
    root: u64,
    rows: &[(Series, i64, f64)],
) -> Result<String, String> {
    let node = dag
        .nodes
        .iter()
        .find(|n| u64::from(n.id.0) == root)
        .unwrap();
    let PostAsapOperatorPayload::SummaryAgg {
        family,
        input,
        reduction,
        grouping,
        ..
    } = &node.payload
    else {
        unreachable!()
    };
    let source_node = dag
        .nodes
        .iter()
        .find(|n| u64::from(n.id.0) == source)
        .unwrap();
    let keys = match reduction {
        Reduction::Reduce(keys) => keys
            .keys()
            .iter()
            .map(|i| source_node.output_schema.fields[*i].name.clone())
            .collect(),
        Reduction::PerEntity => vec![],
    };
    let stored_only = matches!(family, FieldDataType::Sketch(kind, _)
        if kind.algorithm() == &asap_types::post_asap::SketchAlgorithm::Cms);
    if stored_only || asap_physical_operators::capability::validate_native_family(family).is_err() {
        // Families without a native state (e.g. UnivMon), or with native
        // stored state only (plain CMS), are outside precompute execution;
        // their compile must fail.
        assert!(precompute::compile(dag, &[source], &[root]).is_err());
        return Err(format!("{family:?}"));
    }
    let actual = execute(dag, source, root, rows);
    let label = match family {
        FieldDataType::ExactAggregate(kind, _) => format!("{kind:?}"),
        FieldDataType::Sketch(kind, _) => format!("{:?}", kind.algorithm()),
        other => format!("{other:?}"),
    };
    if let FieldDataType::Sketch(kind, _) = family {
        if let (Some(keyed), false) = (&input.item, kind.algorithm() == &SketchAlgorithm::Hll) {
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
        BTreeMap::<Series, Box<dyn asap_physical_operators::factory::AccumulatorUpdater>>::new();
    for (labels, time, value) in rows {
        let updater = expected
            .entry(population(reduction, &keys, labels))
            .or_insert_with(|| create_planner_accumulator(family, input, grouping).unwrap());
        let unit = input.item.is_some();
        updater.update_single(if unit { *value } else { weight(input, *value) }, *time);
    }
    assert_eq!(actual.len(), expected.len(), "{query}");
    for (labels, state) in actual {
        let reference = expected[&labels].snapshot_accumulator();
        assert_eq!(
            readouts(state.as_ref(), family),
            readouts(reference.as_ref(), family),
            "{query}: {labels:?}"
        );
    }
    Ok(label)
}

// Every raw-input summary selected by Planner compiles over raw sample rows,
// and each population's estimates equal feeding its kernel sample by sample.
#[test]
fn raw_sample_summaries_compile_and_match_their_kernels() {
    let exact = AccuracyTarget::Exact;
    let sketch = AccuracyTarget::Epsilon(0.02);
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
        ("distinct_over_time(m[5m])", &sketch),
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
            let dag = compile_post_asap_dag(&candidate).unwrap();
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
        "CountSketchWithHeap",
    ] {
        assert!(
            families.contains(family),
            "no {family} fixture: {families:?}"
        );
    }
}

/// Replace the raw summary of `sum by (service) (sum_over_time(m[5m]))` with
/// another update, keeping its raw input and reduction.
fn grouped_raw_summary(family: FieldDataType, input: SummaryUpdate) -> (PostAsapDAG, u64, u64) {
    let candidate = candidates(
        "sum by (service) (sum_over_time(m[5m]))",
        AccuracyTarget::Exact,
    )
    .pop()
    .unwrap();
    let mut dag = compile_post_asap_dag(&candidate).unwrap();
    let (source, root) = raw_summaries(&dag)[0];
    let node = dag
        .nodes
        .iter_mut()
        .find(|n| u64::from(n.id.0) == root)
        .unwrap();
    let PostAsapOperatorPayload::SummaryAgg {
        family: old,
        input: update,
        ..
    } = &mut node.payload
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
    for edge in dag
        .edges
        .iter_mut()
        .filter(|e| u64::from(e.producer.0) == root)
    {
        edge.intermediate_schema = schema.clone();
    }
    (dag, source, root)
}

// Keyed heaps over raw samples resolve items from the series label set and
// estimate each item's exact total; invalid weight contracts do not compile.
#[test]
fn raw_sample_heaps_resolve_items_from_labels() {
    use asap_types::post_asap::{
        GroupingStrategy::PerSubpopulationInstance, NonNegativeWeightProof, SketchKind,
        SketchParams, WeightDomain,
    };
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
    let identity = SummaryUpdate {
        item: Some(SummaryInputExpr::EntityIdentity(
            EntityIdentity::PromqlLabelSet { excluding: vec![] },
        )),
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
        let (dag, source, root) = grouped_raw_summary(family, input);
        assert!(check("heap", &dag, source, root, &rows).is_ok());
    }
    let signed_cms = grouped_raw_summary(cms.clone(), by_instance.clone());
    assert!(precompute::compile(&signed_cms.0, &[signed_cms.1], &[signed_cms.2]).is_err());
    let derivative = grouped_raw_summary(
        cms,
        SummaryUpdate {
            weight_domain: WeightDomain::NonNegative {
                proof: NonNegativeWeightProof::ResetAwareCounterDerivative,
            },
            ..by_instance.clone()
        },
    );
    assert!(
        precompute::compile(&derivative.0, &[derivative.1], &[derivative.2]).is_err(),
        "raw samples are cumulative counters, not their derivative"
    );
    // A scan's time column is not a label; it cannot silently read as empty.
    let time_item = grouped_raw_summary(
        heap(
            SketchAlgorithm::CountSketchWithHeap,
            SketchParams::CountSketchWithHeap {
                width: 64,
                depth: 3,
                heap_size: 8,
            },
        ),
        SummaryUpdate {
            item: Some(SummaryInputExpr::Column(ColumnRef::Named("ts".into()))),
            ..by_instance
        },
    );
    assert!(precompute::compile(&time_item.0, &[time_item.1], &[time_item.2]).is_err());
}

// `without` grouping over raw samples drops the listed labels and `__name__`.
#[test]
fn raw_sample_without_grouping_drops_labels_and_name() {
    use asap_types::pre_asap::query_expr::GroupKeys;
    let family =
        FieldDataType::ExactAggregate(ExactKind::Sum, asap_types::post_asap::ExactParams::Sum);
    let (mut dag, source, root) =
        grouped_raw_summary(family, SummaryUpdate::column(ColumnRef::SampleValue));
    let service = dag
        .nodes
        .iter()
        .find(|n| u64::from(n.id.0) == source)
        .unwrap()
        .output_schema
        .fields
        .iter()
        .position(|f| f.name == "service")
        .unwrap();
    let node = dag
        .nodes
        .iter_mut()
        .find(|n| u64::from(n.id.0) == root)
        .unwrap();
    let PostAsapOperatorPayload::SummaryAgg { reduction, .. } = &mut node.payload else {
        unreachable!()
    };
    *reduction = Reduction::Reduce(GroupKeys::without(vec![service]));
    assert_eq!(
        check("without", &dag, source, root, &samples()),
        Ok("Sum".into())
    );
}
