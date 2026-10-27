use asap_types::ir::operator::{AggIntent, Reduction, Source};
use asap_types::ir::properties::{AccuracyGuarantee, ExecutionTiming};
use asap_types::ir::scalar::ColumnRef;
use asap_types::ir::schema::{DataType, Field, FieldDataType, Schema};
use asap_types::ir::schema::{ExactKind, ExactParams, GroupingStrategy, SummaryUpdate};
use asap_types::ir::{ASAPOp, NonASAPOp, Operator, OperatorNode};
use std::rc::Rc;

fn scan(key_type: DataType, name: &str) -> Rc<OperatorNode> {
    OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Scan {
        source: Source::Table {
            table_ref: "t".into(),
        },
        predicates: vec![],
        schema: Schema::new(vec![
            Field::plain(name, key_type, false),
            Field::plain("value", DataType::Float64, false),
        ]),
    }))
    .unwrap()
}
fn aggregate(child: Rc<OperatorNode>, asap: bool) -> Rc<OperatorNode> {
    let operator = if asap {
        Operator::ASAP(ASAPOp::SummaryAgg {
            child,
            family: FieldDataType::ExactAggregate(ExactKind::Sum, ExactParams::Sum),
            input: SummaryUpdate::column(ColumnRef::Named("value".into())),
            reduction: Reduction::by(vec![0]),
            grouping: GroupingStrategy::default(),
            filter: None,
        })
    } else {
        Operator::NonASAP(NonASAPOp::Aggregate {
            child,
            reduction: Reduction::by(vec![0]),
            measures: vec![AggIntent::Sum { col: Some(1) }],
            output_names: vec![],
            filters: vec![],
            having: None,
        })
    };
    OperatorNode::new_shared(operator).unwrap()
}

/// Rewrites follow changed input types and inherited names for either category.
#[test]
fn rebuilding_rederives_schema_for_both_categories() {
    for asap in [false, true] {
        let original = aggregate(scan(DataType::Int64, "key"), asap);
        original.validate_structure().unwrap();
        let replacement = scan(DataType::Utf8, "new_key");
        let rebuilt = Rc::new(original.with_new_children(|_| replacement.clone()).unwrap());
        assert_eq!(rebuilt.schema, rebuilt.operator.output_schema().unwrap());
        rebuilt.validate_structure().unwrap();
    }
}

/// Naming overrides survive rewrites without freezing types or assessed properties.
#[test]
fn rebuilding_preserves_only_explicit_naming_overrides() {
    for asap in [false, true] {
        let original = aggregate(scan(DataType::Int64, "key"), asap);
        let mut schema = original.schema.clone();
        schema.fields[0].name = "alias".into();
        schema.fields[0].table = Some("result".into());
        let renamed = OperatorNode::with_schema(original.operator.clone(), schema)
            .with_guarantee(Some(AccuracyGuarantee::exact("fixture")))
            .with_timing(Some(ExecutionTiming::QueryTime));
        let original = Rc::new(renamed);
        original.validate_structure().unwrap();
        let replacement = scan(DataType::Utf8, "new_key");
        let rebuilt = Rc::new(original.with_new_children(|_| replacement.clone()).unwrap());
        assert_eq!(rebuilt.schema.fields[0].name, "alias");
        assert_eq!(rebuilt.schema.fields[0].table.as_deref(), Some("result"));
        assert_eq!(
            rebuilt.schema.fields[0].plain_dtype(),
            Some(&DataType::Utf8)
        );
        assert!(rebuilt.guarantee.is_none());
        assert!(rebuilt.timing.is_none());
        rebuilt.validate_structure().unwrap();
    }
}

/// Custom names never authorize changes to structural schema metadata.
#[test]
fn validation_rejects_structural_overrides_for_both_categories() {
    for asap in [false, true] {
        let original = aggregate(scan(DataType::Timestamp, "key"), asap);
        original.validate_structure().unwrap();
        let mut invalid = vec![];
        let mut schema = original.schema.clone();
        schema.unique_keys = vec![vec![1]];
        invalid.push(schema);
        let mut schema = original.schema.clone();
        schema.time_index = Some(0); // In-range Timestamp, but not the derived time axis.
        invalid.push(schema);
        let mut schema = original.schema.clone();
        schema.closed = !schema.closed;
        invalid.push(schema);
        let mut schema = original.schema.clone();
        schema.fields[0].nullable = !schema.fields[0].nullable;
        invalid.push(schema);
        let mut schema = original.schema.clone();
        schema.fields[0].dtype = FieldDataType::Plain(DataType::Utf8);
        invalid.push(schema);
        let mut schema = original.schema.clone();
        schema.fields.pop();
        invalid.push(schema);
        for schema in invalid {
            let forged = Rc::new(OperatorNode::with_schema(original.operator.clone(), schema));
            assert!(
                forged.validate_structure().is_err(),
                "accepted structural override: {:?}",
                forged.schema
            );
        }
    }
}

/// Passthrough rewrites derive metadata and arity, but cannot guess alias positions.
#[test]
fn rebuilding_updates_metadata_and_requires_new_aliases_after_arity_changes() {
    use asap_types::ir::operator::GroupKeys;
    let input = scan(DataType::Timestamp, "key");
    let original = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Limit {
        n: Some(10),
        offset: 0,
        partition_by: GroupKeys::by(vec![]),
        child: input,
    }))
    .unwrap();
    let mut replacement_schema = original.schema.clone();
    replacement_schema.time_index = Some(0);
    replacement_schema.unique_keys = vec![vec![0]];
    replacement_schema.closed = false;
    replacement_schema
        .fields
        .push(Field::plain("extra", DataType::Int64, true));
    let replacement = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Scan {
        source: Source::Table {
            table_ref: "t".into(),
        },
        predicates: vec![],
        schema: replacement_schema.clone(),
    }))
    .unwrap();
    let rebuilt = Rc::new(original.with_new_children(|_| replacement.clone()).unwrap());
    assert_eq!(rebuilt.schema, replacement_schema);
    rebuilt.validate_structure().unwrap();

    let mut names = original.schema.clone();
    names.fields[0].name = "alias".into();
    let named = OperatorNode::with_schema(original.operator.clone(), names);
    assert!(named.with_new_children(|_| replacement.clone()).is_err());
}

/// Maintaining membership and finalizing values preserve identity/time metadata.
#[test]
fn summary_transitions_preserve_structural_metadata() {
    use asap_types::ir::operator::maintained_population::{MaintainedPopulation, PopulationInput};
    use asap_types::ir::operator::GroupKeys;
    let mut schema = scan(DataType::Timestamp, "key").schema.clone();
    schema.closed = true;
    schema.time_index = Some(0);
    schema.unique_keys = vec![vec![0]];
    let source = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Scan {
        source: Source::Table {
            table_ref: "t".into(),
        },
        predicates: vec![],
        schema: schema.clone(),
    }))
    .unwrap();
    let maintained = OperatorNode::new_shared(Operator::ASAP(ASAPOp::MaintainPopulation {
        child: source.clone(),
        population: MaintainedPopulation {
            input: PopulationInput::Rows {
                input: source.clone(),
                value_column: 1,
                grouping: GroupKeys::by(vec![0]),
            },
            max_k: 1,
            quantiles: false,
        },
    }))
    .unwrap();
    assert_eq!(maintained.schema, schema);
    maintained.validate_structure().unwrap();

    let state = aggregate(source, true);
    let finalized = OperatorNode::new_shared(Operator::ASAP(ASAPOp::FinalizeExactAccumulator {
        child: state.clone(),
    }))
    .unwrap();
    assert_eq!(finalized.schema.unique_keys, state.schema.unique_keys);
    assert_eq!(finalized.schema.closed, state.schema.closed);
    assert_eq!(finalized.schema.time_index, state.schema.time_index);
    assert!(finalized.schema.is_all_plain());
    finalized.validate_structure().unwrap();
}

/// The selector `a` as PromQL lowers it: a 1-second instant window over an
/// open series scan with a `job` label.
fn series_selector() -> Rc<OperatorNode> {
    use asap_types::ir::TimeRangeKind;
    let scan = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Scan {
        source: Source::TimeSeries { metric: "a".into() },
        predicates: vec![],
        schema: Schema::with_time_index(
            vec![
                Field::plain("time", DataType::Timestamp, false),
                Field::plain("value", DataType::Float64, false),
                Field::plain("job", DataType::Utf8, false),
            ],
            0,
            vec![],
        ),
    }))
    .unwrap();
    OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::TimeRange {
        range: std::time::Duration::from_secs(1),
        kind: TimeRangeKind::Instant,
        child: scan,
    }))
    .unwrap()
}

/// `topk(5, a)` read from a current-series population of `a`, built by hand.
fn current_series_topk() -> Rc<OperatorNode> {
    use asap_types::ir::operator::maintained_population::*;
    let selector = series_selector();
    let population = OperatorNode::new_shared(Operator::ASAP(ASAPOp::MaintainPopulation {
        child: selector,
        population: MaintainedPopulation {
            input: PopulationInput::CurrentSeries(CurrentSeriesInput {
                metric: "a".into(),
                matchers: vec![],
                grouping: vec![],
                without: false,
                lookback_ms: 1_000,
            }),
            max_k: 5,
            quantiles: false,
        },
    }))
    .unwrap();
    OperatorNode::new_shared(Operator::ASAP(ASAPOp::EvaluatePopulation {
        child: population,
        evaluation: PopulationStatistic::TopK { k: 5 },
    }))
    .unwrap()
}

/// Time `root` under the default materialization assignment, which runs the
/// data-state and population-contract validation, and export it.
fn compile_physical_asap_dag(root: &Rc<OperatorNode>) -> Result<(), String> {
    use asap_types::ir::{apply_materialization_timings, MaterializationAssignment, TimingMemo};
    root.validate_structure().map_err(|e| e.to_string())?;
    let timed = apply_materialization_timings(
        root,
        &MaterializationAssignment::default(),
        &mut TimingMemo::new(),
    )
    .map_err(|e| format!("{e:?}"))?;
    asap_types::ir::physical_export::compile_physical_asap_dag(&timed)
        .map_err(|e| format!("{e:?}"))?;
    Ok(())
}

/// A table-row population maintains a numeric value column: one that names
/// a label column fails validation.
#[test]
fn a_row_population_over_a_label_column_is_invalid() {
    use asap_types::ir::operator::maintained_population::{MaintainedPopulation, PopulationInput};
    use asap_types::ir::operator::GroupKeys;
    let mut schema = scan(DataType::Utf8, "job").schema.clone();
    schema.closed = true;
    let source = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Scan {
        source: Source::Table {
            table_ref: "t".into(),
        },
        predicates: vec![],
        schema,
    }))
    .unwrap();
    let population = |value_column| {
        Rc::new(OperatorNode::with_schema(
            Operator::ASAP(ASAPOp::MaintainPopulation {
                child: source.clone(),
                population: MaintainedPopulation {
                    input: PopulationInput::Rows {
                        input: source.clone(),
                        value_column,
                        grouping: GroupKeys::by(vec![0]),
                    },
                    max_k: 0,
                    quantiles: false,
                },
            }),
            source.schema.clone(),
        ))
    };
    population(1).validate_structure().unwrap();
    assert!(population(0).validate_structure().is_err());
}

/// A current-series population matches its selector only with the
/// selector's own lookback, so members expire when the selector stops
/// seeing them.
#[test]
fn a_current_series_population_matches_only_its_selectors_lookback() {
    use asap_types::ir::operator::maintained_population::CurrentSeriesInput;
    let spec = CurrentSeriesInput {
        metric: "a".into(),
        matchers: vec![],
        grouping: vec![],
        without: false,
        lookback_ms: 1_000,
    };
    let selector = series_selector();
    assert!(spec.matches_node(&selector));
    let wrong = CurrentSeriesInput {
        lookback_ms: 300_000,
        ..spec
    };
    assert!(!wrong.matches_node(&selector));
}

/// Population timing is a materialization choice: the population follows its
/// assignment (`NotMaterialized` by default), while its evaluation always
/// runs at query time.
#[test]
fn population_timing_follows_its_materialization() {
    use asap_types::ir::physical_export::compile_physical_asap_dag as export_timed;
    use asap_types::ir::properties::ExecutionTiming::{IngestionTime, QueryTime};
    use asap_types::ir::{apply_materialization_timings, MaterializationAssignment, TimingMemo};
    use asap_types::physical::{Materialization, Medium};
    let candidate = current_series_topk();
    let Some(ASAPOp::EvaluatePopulation {
        child: population, ..
    }) = candidate.asap()
    else {
        unreachable!()
    };
    let on_disk = Materialization::MatAtIngestion(Medium::Disk);
    let mut at_ingestion = MaterializationAssignment::default();
    at_ingestion.set(population, on_disk);
    for (assignment, timing, materialization) in [
        (
            MaterializationAssignment::default(),
            QueryTime,
            Materialization::NotMaterialized,
        ),
        (at_ingestion, IngestionTime, on_disk),
    ] {
        let timed =
            apply_materialization_timings(&candidate, &assignment, &mut TimingMemo::new()).unwrap();
        let dag = export_timed(&timed).unwrap();
        let [population, evaluation] = &dag.nodes[dag.nodes.len() - 2..] else {
            unreachable!()
        };
        assert_eq!(population.output_state.timing, timing);
        assert_eq!(population.materialization, materialization);
        assert_eq!(evaluation.output_state.timing, QueryTime);
    }
    let mut evaluation_at_ingestion = (*candidate).clone();
    evaluation_at_ingestion.timing = Some(IngestionTime);
    assert!(compile_physical_asap_dag(&Rc::new(evaluation_at_ingestion)).is_err());
}

/// An evaluation cannot exceed its population's contract (`max_k`), and a
/// population cannot claim a source it does not read.
#[test]
fn malformed_population_dags_fail_closed() {
    use asap_types::ir::operator::maintained_population::{PopulationInput, PopulationStatistic};
    let candidate = current_series_topk();
    compile_physical_asap_dag(&candidate).expect("the unmodified candidate is legal");
    let mut bad = (*candidate).clone();
    let Operator::ASAP(ASAPOp::EvaluatePopulation { evaluation, .. }) = &mut bad.operator else {
        unreachable!()
    };
    *evaluation = PopulationStatistic::TopK { k: 6 };
    assert!(compile_physical_asap_dag(&Rc::new(bad.clone())).is_err());
    let Operator::ASAP(ASAPOp::EvaluatePopulation { child, evaluation }) = &mut bad.operator else {
        unreachable!()
    };
    *evaluation = PopulationStatistic::TopK { k: 5 };
    let producer = Rc::make_mut(child);
    let Operator::ASAP(ASAPOp::MaintainPopulation { population, .. }) = &mut producer.operator
    else {
        unreachable!()
    };
    let PopulationInput::CurrentSeries(spec) = &mut population.input else {
        unreachable!()
    };
    spec.metric = "b".into();
    assert!(compile_physical_asap_dag(&Rc::new(bad)).is_err());
}
