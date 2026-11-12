//! The `Memo` operations other than the equality they rely on: storing
//! candidates ([`Memo::add_candidate`]), parents and queries, requirements,
//! and output schemas of pending expressions, on the concepts doc's KLL
//! example (section 1: G1 the latency scan, G2 its KLL state).

use super::*;
use crate::ir::operator::arguments::Reduction;
use crate::ir::schema::FieldDataType;
use crate::ir::NonASAPOp;
use crate::stages::logical::group::MemoGroupAsInput;
use crate::stages::LogicalPropertiesError;
use crate::summary_type::{SketchAlgorithm, SketchParams, SketchStatistic};
use crate::test_support::memo_builders::{query, try_candidate};
use crate::test_support::memo_fixtures::{readout, scan, state, state_grouped_by};

/// `Project(input)` keeping its first column.
fn project_first_column(input: MemoGroupId) -> Operator<MemoGroupId> {
    Operator::NonASAP(NonASAPOp::Project {
        cols: vec![crate::ir::ProjectItem {
            alias: None,
            expr: crate::ir::ScalarExpr::Column(0),
        }],
        child: input,
    })
}

/// KLL `k`.
pub(super) fn kll_k(k: u32) -> Option<SketchParams> {
    Some(SketchParams::Kll { k })
}

/// A new group is a parent of each of its input groups, listed once.
#[test]
fn find_or_add_group_lists_the_new_group_among_its_inputs_parents() {
    let mut memo = Memo::new(&DataWorkload::default());
    let g1 = memo
        .find_or_add_group(scan("latency", &[], vec![]))
        .unwrap();
    let g2 = memo
        .find_or_add_group(state(FieldDataType::Sketch(SketchAlgorithm::Kll), g1))
        .unwrap();
    memo.find_or_add_group(state(FieldDataType::Sketch(SketchAlgorithm::Kll), g1))
        .unwrap();
    assert_eq!(memo.groups()[g1.0 as usize].parent_groups(), &[g2]);
    assert!(memo.groups()[g2.0 as usize].parent_groups().is_empty());
}

/// A new candidate makes its group a parent of each group it reads, and
/// the group's queries reach those groups and every group below them.
#[test]
fn add_candidate_maintains_parents_and_queries() {
    // Query 0 is G3 = SummaryEstimate[q=0.95](G2) over G2 = KLL(G1).
    let mut memo = Memo::new(&DataWorkload::default());
    let g1 = memo
        .find_or_add_group(scan("latency", &[], vec![]))
        .unwrap();
    let g2 = memo
        .find_or_add_group(state(FieldDataType::Sketch(SketchAlgorithm::Kll), g1))
        .unwrap();
    let g3 = memo
        .find_or_add_group(readout(SketchStatistic::Quantile { q: 0.95 }, g2))
        .unwrap();
    assert_eq!(query(&mut memo, g3, None), 0);
    // A rule adds a state group G5 over another scan G4: no query
    // reaches them yet.
    let g4 = memo
        .find_or_add_group(scan("latency_copy", &[], vec![]))
        .unwrap();
    let g5 = memo
        .find_or_add_group(state(FieldDataType::Sketch(SketchAlgorithm::Kll), g4))
        .unwrap();
    assert!(memo.groups()[g5.0 as usize].reached_by_queries().is_empty());
    // G3 gains a candidate reading G5: G5 and G4 now serve query 0.
    try_candidate(
        &mut memo,
        g3,
        readout(SketchStatistic::Quantile { q: 0.95 }, g5),
        None,
    )
    .unwrap();
    assert_eq!(memo.groups()[g5.0 as usize].parent_groups(), &[g3]);
    assert_eq!(memo.groups()[g4.0 as usize].parent_groups(), &[g5]);
    assert_eq!(memo.groups()[g5.0 as usize].reached_by_queries(), &[0]);
    assert_eq!(memo.groups()[g4.0 as usize].reached_by_queries(), &[0]);
    // Query 1 is G6 = Project(G3): it reaches G3 and everything below
    // it, through expressions and candidates, in ascending order.
    let g6 = memo.find_or_add_group(project_first_column(g3)).unwrap();
    assert_eq!(query(&mut memo, g6, None), 1);
    for group in [g1, g2, g3, g4, g5] {
        assert_eq!(
            memo.groups()[group.0 as usize].reached_by_queries(),
            &[0, 1],
            "group {group:?}"
        );
    }
    assert_eq!(memo.groups()[g3.0 as usize].parent_groups(), &[g6]);
}

/// A candidate that computes another result than its group's (a
/// readout offered for a state group) is rejected and changes nothing.
#[test]
fn add_candidate_rejects_a_candidate_whose_output_differs_from_its_group() {
    let mut memo = Memo::new(&DataWorkload::default());
    let g1 = memo
        .find_or_add_group(scan("latency", &[], vec![]))
        .unwrap();
    let g2 = memo
        .find_or_add_group(state(FieldDataType::Sketch(SketchAlgorithm::Kll), g1))
        .unwrap();
    let before = format!("{memo:?}");
    assert!(matches!(
        try_candidate(&mut memo, g2, readout(SketchStatistic::Quantile { q: 0.5 }, g2), None),
        Err(CandidateError::OutputDiffersFromGroup { group }) if group == g2
    ));
    assert_eq!(
        format!("{memo:?}"),
        before,
        "a rejected candidate changes nothing"
    );
}

/// A candidate whose expression fails input validation (a readout of
/// raw rows instead of summary state) is rejected and changes nothing.
#[test]
fn add_candidate_rejects_a_candidate_whose_inputs_fail_validation() {
    let mut memo = Memo::new(&DataWorkload::default());
    let g1 = memo
        .find_or_add_group(scan("latency", &[], vec![]))
        .unwrap();
    let g2 = memo
        .find_or_add_group(state(FieldDataType::Sketch(SketchAlgorithm::Kll), g1))
        .unwrap();
    let g3 = memo
        .find_or_add_group(readout(SketchStatistic::Quantile { q: 0.95 }, g2))
        .unwrap();
    let over_raw_rows =
        readout(SketchStatistic::Quantile { q: 0.95 }, g1).map_children(|&group_id| {
            MemoGroupAsInput {
                memo: &memo,
                group_id,
            }
        });
    assert!(
        over_raw_rows.output_schema().is_ok(),
        "fixture: the schema derives; only input validation rejects it"
    );
    let before = format!("{memo:?}");
    assert!(matches!(
        try_candidate(
            &mut memo,
            g3,
            readout(SketchStatistic::Quantile { q: 0.95 }, g1),
            None
        ),
        Err(CandidateError::NotValidOverInputGroups(_))
    ));
    assert_eq!(
        format!("{memo:?}"),
        before,
        "a rejected candidate changes nothing"
    );
}

/// An expression with no output schema (a group-by column out of range)
/// is an error, and no group is added.
#[test]
fn find_or_add_group_returns_an_error_for_an_expression_with_no_schema() {
    let mut memo = Memo::new(&DataWorkload::default());
    let g1 = memo
        .find_or_add_group(scan("latency", &[], vec![]))
        .unwrap();
    let out_of_range = Operator::NonASAP(NonASAPOp::Aggregate {
        reduction: Reduction::by(vec![9]),
        measures: vec![crate::ir::operator::agg_intent::AggIntent::Sum { col: Some(1) }],
        output_names: vec![],
        filters: vec![],
        having: None,
        child: g1,
    });
    assert!(memo.find_or_add_group(out_of_range).is_err());
    assert_eq!(memo.groups().len(), 1);
    assert!(memo.groups()[g1.0 as usize].parent_groups().is_empty());
}

/// A KLL state over the latency scan and its p99 readout, both pending: the
/// readout's schema is the one its group gets once both are added, and
/// nothing is added while deriving it.
#[test]
fn output_schema_of_a_pending_expression_is_its_groups_schema() {
    let mut memo = Memo::new(&DataWorkload::default());
    let latency = memo
        .find_or_add_group(scan("latency", &[], vec![]))
        .unwrap();
    let state_expression = state(FieldDataType::Sketch(SketchAlgorithm::Kll), latency);
    let pending_state = MemoGroupId(memo.groups().len() as u32);
    let readout_expression = readout(SketchStatistic::Quantile { q: 0.99 }, pending_state);
    let before = format!("{memo:?}");
    let state_schema = memo
        .output_schema_of_pending_expression(&[], &state_expression)
        .unwrap();
    let readout_schema = memo
        .output_schema_of_pending_expression(
            &[(state_expression.clone(), state_schema.clone())],
            &readout_expression,
        )
        .unwrap();
    assert_eq!(format!("{memo:?}"), before);
    let state_group = memo.find_or_add_group(state_expression).unwrap();
    let readout_group = memo
        .find_or_add_group(readout(SketchStatistic::Quantile { q: 0.99 }, state_group))
        .unwrap();
    let schema_of = |group: MemoGroupId| &memo.groups()[group.0 as usize].group_properties.schema;
    assert_eq!(schema_of(state_group), &state_schema);
    assert_eq!(schema_of(readout_group), &readout_schema);
}

/// A pending expression whose schema does not derive (a readout of a scan,
/// not of a summary state) is an error, as `find_or_add_group` gives it.
#[test]
fn output_schema_of_a_pending_expression_rejects_what_find_or_add_group_rejects() {
    let mut memo = Memo::new(&DataWorkload::default());
    let latency = memo
        .find_or_add_group(scan("latency", &[], vec![]))
        .unwrap();
    let bad = readout(SketchStatistic::Quantile { q: 0.99 }, latency);
    assert!(memo.output_schema_of_pending_expression(&[], &bad).is_err());
    assert!(memo.find_or_add_group(bad).is_err());
}

/// One readout's required accuracy.
fn required(max_error: f64) -> Vec<LogicalRequirements> {
    vec![LogicalRequirements {
        required_accuracy: Some(crate::stages::RequiredAccuracy {
            metric: crate::accuracy::AccuracyMetric::Rank,
            max_error,
            failure_probability: 1.0,
        }),
    }]
}

/// `Memo::set_candidate_input_requirements`
/// replaces $r^{\log}$ of the one candidate it names; two readouts of one
/// state with different required accuracy are two candidates.
#[test]
fn set_candidate_input_requirements_replaces_only_the_named_candidate() {
    let mut memo = Memo::new(&DataWorkload::default());
    let g1 = memo
        .find_or_add_group(scan("latency", &[], vec![]))
        .unwrap();
    let g2 = memo
        .find_or_add_group(state(FieldDataType::Sketch(SketchAlgorithm::Kll), g1))
        .unwrap();
    let p90 = || readout(SketchStatistic::Quantile { q: 0.9 }, g2);
    let g3 = memo.find_or_add_group(p90()).unwrap();
    let first = try_candidate(&mut memo, g3, p90(), None).unwrap();
    memo.set_candidate_input_requirements(first, required(0.01));
    let second = try_candidate(&mut memo, g3, p90(), None).unwrap();
    assert_ne!(first, second, "an unset copy is a new candidate");
    memo.set_candidate_input_requirements(second, required(0.02));
    let requirements: Vec<_> = memo.groups()[g3.0 as usize]
        .candidates
        .iter()
        .map(|c| c.input_requirements.clone())
        .collect();
    assert_eq!(requirements, vec![required(0.01), required(0.02)]);
}

/// A merge of KLL states carries the θ of the
/// states it reads. Without one it is rejected; `merge[k=547]` reads only
/// the k = 547 build of its input group.
#[test]
fn a_merge_carries_the_parameters_of_the_states_it_reads() {
    let mut memo = Memo::new(&DataWorkload::default());
    let g1 = memo
        .find_or_add_group(scan("latency", &[], vec![]))
        .unwrap();
    let kll_state = || state(FieldDataType::Sketch(SketchAlgorithm::Kll), g1);
    let g2 = memo.find_or_add_group(kll_state()).unwrap();
    for k in [269, 547] {
        try_candidate(&mut memo, g2, kll_state(), kll_k(k)).unwrap();
    }
    let merge = Operator::ASAP(crate::ir::ASAPOp::SummaryMerge {
        children: vec![g2],
        group_by: None,
    });
    let g3 = memo.find_or_add_group(merge.clone()).unwrap();
    assert!(matches!(
        try_candidate(&mut memo, g3, merge.clone(), None),
        Err(CandidateError::LogicalProperties(
            LogicalPropertiesError::SketchBuildWithoutParameters
        ))
    ));
    let merged = try_candidate(&mut memo, g3, merge, kll_k(547)).unwrap();
    assert_eq!(
        memo.producers(merged, 0),
        vec![CandidateForMemoGroupId {
            group: g2,
            index_in_group: 1
        }]
    );
}

/// One requirement per input group, or a panic.
#[test]
#[should_panic(expected = "one input requirement per input group")]
fn set_candidate_input_requirements_panics_on_a_length_mismatch() {
    let mut memo = Memo::new(&DataWorkload::default());
    let g1 = memo
        .find_or_add_group(scan("latency", &[], vec![]))
        .unwrap();
    let kll_state = || state(FieldDataType::Sketch(SketchAlgorithm::Kll), g1);
    let g2 = memo.find_or_add_group(kll_state()).unwrap();
    let build = try_candidate(&mut memo, g2, kll_state(), kll_k(269)).unwrap();
    memo.set_candidate_input_requirements(build, Vec::new());
}

/// `add_candidate` checks θ against the group's output, for every operator
/// kind: a sketch state's candidates (a build, a chain, a merge) need θ of
/// its algorithm from its list; any other candidate (an exact build, a
/// readout) has none. No Hydra θ is listed yet, so no Hydra build is valid.
#[test]
fn add_candidate_checks_parameters_against_the_groups_output() {
    use crate::ir::operator::arguments::Reduction;
    use crate::ir::scalar::expr_ir::ColumnRef;
    use crate::ir::ASAPOp;
    use crate::summary_type::{ExactKind, HydraParams, SummaryUpdate};
    use LogicalPropertiesError as Invalid;
    let mut memo = Memo::new(&DataWorkload::default());
    let g1 = memo
        .find_or_add_group(scan("latency", &[], vec![]))
        .unwrap();
    let kll = FieldDataType::Sketch(SketchAlgorithm::Kll);
    let kll_state = memo.find_or_add_group(state(kll.clone(), g1)).unwrap();
    let kll_chain = Operator::ASAP(ASAPOp::SummaryTumblingWindowChain {
        child: g1,
        summary_type: kll.clone(),
        input: SummaryUpdate::column(ColumnRef::SampleValue),
        reduction: Reduction::PerEntity,
        filter: None,
        tumbling_window_width_ms: 5_000,
    });
    let merge = Operator::ASAP(ASAPOp::SummaryMerge {
        children: vec![kll_state],
        group_by: None,
    });
    // k = 270 is KLL's structure but not in its list.
    let parameters = [
        None,
        kll_k(269),
        Some(SketchParams::Cms {
            width: 272,
            depth: 5,
        }),
        Some(SketchParams::Hydra(HydraParams::HydraCms {
            width: 272,
            depth: 5,
            shared_rows: 3,
            shared_columns: 1_024,
        })),
        kll_k(270),
    ];
    let no_parameters = Err(Invalid::ParametersOnExactOperator);
    let needs_parameters = Err(Invalid::SketchBuildWithoutParameters);
    let not_in_list = Err(Invalid::ParametersNotInList);
    let exact = [
        Ok(()),
        no_parameters,
        no_parameters,
        no_parameters,
        no_parameters,
    ];
    // Another algorithm's parameters are not in KLL's list either.
    let kll_state_output = [
        needs_parameters,
        Ok(()),
        not_in_list,
        not_in_list,
        not_in_list,
    ];
    // Per operator: the result for each of `parameters`.
    let table = [
        (state(kll, g1), kll_state_output),
        (
            state(FieldDataType::Sketch(SketchAlgorithm::HydraCms), g1),
            [
                needs_parameters,
                not_in_list,
                not_in_list,
                not_in_list,
                not_in_list,
            ],
        ),
        (state(FieldDataType::exact(ExactKind::Sum), g1), exact),
        (kll_chain, kll_state_output),
        (
            readout(SketchStatistic::Quantile { q: 0.5 }, kll_state),
            exact,
        ),
        (merge, kll_state_output),
    ];
    for (operator, by_parameters) in table {
        let group = memo.find_or_add_group(operator.clone()).unwrap();
        for (parameters, expected) in parameters.iter().zip(by_parameters) {
            let added = try_candidate(&mut memo, group, operator.clone(), parameters.clone());
            let added = match added {
                Ok(_) => Ok(()),
                Err(CandidateError::LogicalProperties(error)) => Err(error),
                Err(error) => panic!("{error}"),
            };
            assert_eq!(added, expected, "{:?} {parameters:?}", operator.kind_name());
        }
    }
}

/// A readout returns at most the series scanned below it, the declared
/// distinct values of its state's keys, and one value without keys: over
/// `latency` with 1000 series and 20 declared jobs, a per-series KLL
/// readout returns at most 1000 values, one by `job` 20, one with no key 1.
#[test]
fn max_values_per_evaluation_bounds_a_readout_by_its_states_keys() {
    let data = crate::workload::DataWorkload {
        sources: [(
            "latency".to_string(),
            crate::workload::SourceFacts {
                series_count: Some(1000),
                distinct_values_per_label: [("job".to_string(), 20)].into(),
                ..Default::default()
            },
        )]
        .into(),
        ..Default::default()
    };
    let mut memo = Memo::new(&data);
    let input = memo
        .find_or_add_group(scan(
            "latency",
            &[("job", crate::ir::schema::DataType::Utf8)],
            vec![],
        ))
        .unwrap();
    let job = memo.groups()[input.0 as usize]
        .group_properties
        .schema
        .fields
        .iter()
        .position(|field| field.name == "job")
        .expect("the scan lists job");
    for (reduction, values) in [
        (Reduction::PerEntity, 1000),
        (Reduction::by(vec![job]), 20),
        (Reduction::by(vec![]), 1),
    ] {
        let state = memo
            .find_or_add_group(state_grouped_by(
                FieldDataType::Sketch(SketchAlgorithm::Kll),
                reduction,
                input,
            ))
            .unwrap();
        let quantile = memo
            .find_or_add_group(readout(SketchStatistic::Quantile { q: 0.9 }, state))
            .unwrap();
        assert_eq!(memo.max_values_per_evaluation(quantile), Some(values));
    }
}
