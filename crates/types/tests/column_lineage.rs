//! Column lineage, derived once per memo group from its input groups'
//! (concepts doc `memo-groups-properties-requirements.md`, section 3,
//! "Column lineage"): the doc's examples 1 to 3. Example 4 (PromQL label
//! matchers) is `scan_predicates_lift_and_their_states_merge` in
//! `memo_aggregate_coverage.rs`.
use asap_types::ir::operator::arguments::{AtModifier, JoinKind, Reduction, Source, SourceColumn};
use asap_types::ir::operator::AggIntent;
use asap_types::ir::properties::selection::Constraint;
use asap_types::ir::properties::ColumnLineage;
use asap_types::ir::scalar::expr_ir::{CompareOpKind, ScalarValue};
use asap_types::ir::scalar::ColumnRef;
use asap_types::ir::schema::{DataType, Field, FieldDataType, Schema};
use asap_types::ir::{
    ASAPOp, ExprSemantics, NonASAPOp, Operator, Predicate, ProjectItem, ScalarExpr,
};
use asap_types::stages::{
    CandidateExpression, CandidateForMemoGroup, LogicalProperties, LogicalRequirements, Memo,
    MemoGroupId, OriginOfCandidateForMemoGroup,
};
use asap_types::summary_type::{SketchAlgorithm, SketchStatistic, SummaryUpdate};
use asap_types::test_support::memo_fixtures::{filter, range, shift};
use asap_types::workload::DataWorkload;

fn memo() -> Memo {
    Memo::new(&DataWorkload::default())
}

/// `Scan table` of Utf8 `columns`.
fn scan(table: &str, columns: &[&str]) -> Operator<MemoGroupId> {
    Operator::NonASAP(NonASAPOp::Scan {
        source: Source::Table {
            table_ref: table.into(),
        },
        predicates: vec![],
        schema: Schema::new(
            columns
                .iter()
                .map(|name| Field::plain(*name, DataType::Utf8, false))
                .collect(),
        ),
    })
}

/// `column = value`.
fn equals(column: usize, value: &str) -> ScalarExpr<MemoGroupId> {
    ScalarExpr::Compare {
        left: Box::new(ScalarExpr::Column(column)),
        op: CompareOpKind::Eq,
        right: Box::new(ScalarExpr::Literal(ScalarValue::Utf8(value.into()))),
        semantics: ExprSemantics::Sql,
    }
}

/// `SummaryAgg[HLL](by keys; column)`: the distinct values of `column`.
fn hll(child: MemoGroupId, keys: Vec<usize>, column: &str) -> Operator<MemoGroupId> {
    Operator::ASAP(ASAPOp::SummaryAgg {
        child,
        summary_type: FieldDataType::Sketch(SketchAlgorithm::Hll),
        input: SummaryUpdate::column(ColumnRef::Named(column.into())),
        reduction: Reduction::by(keys),
        filter: None,
    })
}

/// The column `column` of the table `table`.
fn column_of(table: &str, column: &str) -> SourceColumn {
    SourceColumn {
        source: Source::Table {
            table_ref: table.into(),
        },
        column: column.into(),
    }
}

fn source_column(table: &str, column: &str) -> ColumnLineage {
    ColumnLineage::SourceColumn(column_of(table, column))
}

/// The column `column` of the series `latency`.
fn latency_column(column: &str) -> ColumnLineage {
    ColumnLineage::SourceColumn(SourceColumn {
        source: Source::TimeSeries {
            metric: "latency".into(),
        },
        column: column.into(),
    })
}

fn lineage(memo: &Memo, group: MemoGroupId) -> &[ColumnLineage] {
    &memo.groups()[group.0 as usize]
        .group_properties
        .column_lineage
}

/// Example 1: `SELECT f.src, approx_distinct(f.dst) FROM flows f WHERE
/// f.proto = 'tcp' GROUP BY f.src`. The group-by key keeps `flows.src`
/// through the aggregate, the HLL state and its readout; the readout
/// renamed to the aggregate's column name has the aggregate's schema, so
/// `add_candidate` accepts it.
#[test]
fn an_hll_readout_renamed_to_the_aggregate_column_is_its_candidate() {
    let mut memo = memo();
    let g1 = memo
        .find_or_add_group(scan("flows", &["src", "dst", "proto", "ts"]))
        .unwrap();
    let g2 = memo
        .find_or_add_group(filter(equals(2, "tcp"), g1))
        .unwrap();
    let g3 = memo
        .find_or_add_group(Operator::NonASAP(NonASAPOp::Aggregate {
            reduction: Reduction::by(vec![0]),
            measures: vec![AggIntent::Cardinality { cols: vec![1] }],
            output_names: vec!["approx_distinct(dst)".into()],
            filters: vec![],
            having: None,
            child: g2,
        }))
        .unwrap();
    let g4 = memo.find_or_add_group(hll(g2, vec![0], "dst")).unwrap();
    let g5 = memo
        .find_or_add_group(Operator::ASAP(ASAPOp::SummaryEstimate {
            summary_input: g4,
            query: SketchStatistic::Cardinality,
        }))
        .unwrap();
    let flows = ["src", "dst", "proto", "ts"].map(|column| source_column("flows", column));
    assert_eq!(lineage(&memo, g1), flows);
    assert_eq!(lineage(&memo, g2), flows);
    let key_then_computed = [source_column("flows", "src"), ColumnLineage::Computed];
    for group in [g3, g4, g5] {
        assert_eq!(lineage(&memo, group), key_then_computed);
    }
    // The state's selection names the filtered column by its lineage.
    let coverage = memo.groups()[g4.0 as usize]
        .group_properties
        .aggregate_coverage
        .as_ref()
        .expect("a summary state");
    assert_eq!(
        coverage.selection()[0].columns()[&column_of("flows", "proto")],
        Constraint::In(vec![ScalarValue::Utf8("tcp".into())])
    );

    let operator = Operator::NonASAP(NonASAPOp::Project {
        cols: vec![
            ProjectItem {
                alias: Some("src".into()),
                expr: ScalarExpr::Column(0),
            },
            ProjectItem {
                alias: Some("approx_distinct(dst)".into()),
                expr: ScalarExpr::Column(1),
            },
        ],
        child: g5,
    });
    let expression = CandidateExpression { operator };
    let candidate = CandidateForMemoGroup {
        logical_properties: LogicalProperties::new(&expression, None).unwrap(),
        expression,
        input_requirements: vec![LogicalRequirements::default()],
        fused_groups: vec![],
        origin: OriginOfCandidateForMemoGroup::Pass1,
    };
    let id = memo.add_candidate(g3, candidate).unwrap();
    assert_eq!(id.group, g3);
}

/// Example 2: `SELECT source, count(*) FROM (SELECT f.src AS source FROM
/// flows f) GROUP BY source`. The renaming `Project` keeps `flows.src`; a
/// state grouped by `source` has the key lineage of one grouped by `src`
/// over the scan, a selection on the renamed `area` names `flows.region`,
/// and the rollup's group-key check, which compares lineage, passes.
#[test]
fn a_rename_keeps_lineage_and_the_rollup_key_check_compares_it() {
    let mut memo = memo();
    let g1 = memo
        .find_or_add_group(scan("flows", &["src", "dst", "region"]))
        .unwrap();
    let renamed = memo
        .find_or_add_group(Operator::NonASAP(NonASAPOp::Project {
            cols: [("source", 0), ("dst", 1), ("area", 2)]
                .map(|(alias, column)| ProjectItem {
                    alias: Some(alias.into()),
                    expr: ScalarExpr::Column(column),
                })
                .to_vec(),
            child: g1,
        }))
        .unwrap();
    assert_eq!(
        lineage(&memo, renamed),
        [
            source_column("flows", "src"),
            source_column("flows", "dst"),
            source_column("flows", "region"),
        ]
    );

    let by_source = memo
        .find_or_add_group(hll(renamed, vec![0], "dst"))
        .unwrap();
    let by_src = memo.find_or_add_group(hll(g1, vec![0], "dst")).unwrap();
    assert_eq!(lineage(&memo, by_source)[0], lineage(&memo, by_src)[0]);

    let in_us = memo
        .find_or_add_group(filter(equals(2, "us"), renamed))
        .unwrap();
    let by_source_and_area = memo
        .find_or_add_group(hll(in_us, vec![0, 2], "dst"))
        .unwrap();
    let coverage = memo.groups()[by_source_and_area.0 as usize]
        .group_properties
        .aggregate_coverage
        .as_ref()
        .expect("a summary state");
    assert_eq!(
        coverage.selection()[0].columns()[&column_of("flows", "region")],
        Constraint::In(vec![ScalarValue::Utf8("us".into())])
    );

    let rolled_up = memo
        .find_or_add_group(Operator::ASAP(ASAPOp::SummaryMerge {
            children: vec![by_source_and_area],
            group_by: Some(vec![0]),
        }))
        .unwrap();
    let rolled_up = &memo.groups()[rolled_up.0 as usize].group_properties;
    assert_eq!(
        rolled_up.column_lineage,
        [source_column("flows", "src"), ColumnLineage::Computed]
    );
    assert!(rolled_up.aggregate_coverage.is_some());
}

/// Example 3: `SELECT s.region, count(*) FROM sales s JOIN users u ON
/// s.user = u.id GROUP BY s.region`. Both join sides have a `region`
/// column; their lineage tells them apart, so selections on them are
/// different columns.
#[test]
fn join_columns_of_one_name_have_distinct_lineage() {
    let mut memo = memo();
    let sales = memo
        .find_or_add_group(scan("sales", &["user", "region"]))
        .unwrap();
    let users = memo
        .find_or_add_group(scan("users", &["id", "region"]))
        .unwrap();
    let joined = memo
        .find_or_add_group(Operator::NonASAP(NonASAPOp::Join {
            kind: JoinKind::Inner,
            pred: Predicate(ScalarExpr::Compare {
                left: Box::new(ScalarExpr::Column(0)),
                op: CompareOpKind::Eq,
                right: Box::new(ScalarExpr::Column(2)),
                semantics: ExprSemantics::Sql,
            }),
            left: sales,
            right: users,
        }))
        .unwrap();
    assert_eq!(
        lineage(&memo, joined),
        [
            source_column("sales", "user"),
            source_column("sales", "region"),
            source_column("users", "id"),
            source_column("users", "region"),
        ]
    );

    let selected_column = |memo: &mut Memo, region_column: usize| {
        let in_us = memo
            .find_or_add_group(filter(equals(region_column, "us"), joined))
            .unwrap();
        let state = memo.find_or_add_group(hll(in_us, vec![], "id")).unwrap();
        let coverage = memo.groups()[state.0 as usize]
            .group_properties
            .aggregate_coverage
            .as_ref()
            .expect("a summary state");
        let [selection] = coverage.selection() else {
            panic!("one box: {coverage:?}")
        };
        selection.columns().keys().cloned().collect::<Vec<_>>()
    };
    assert_eq!(
        selected_column(&mut memo, 1),
        [column_of("sales", "region")]
    );
    assert_eq!(
        selected_column(&mut memo, 3),
        [column_of("users", "region")]
    );
}

/// A predicate on a computed column is not lifted: no selection can name
/// the column, so it stays in the definition's unlifted operators.
#[test]
fn a_predicate_on_a_computed_column_stays_unlifted() {
    let mut memo = memo();
    let g1 = memo
        .find_or_add_group(scan("flows", &["src", "dst"]))
        .unwrap();
    let computed = memo
        .find_or_add_group(Operator::NonASAP(NonASAPOp::Project {
            cols: vec![
                ProjectItem {
                    alias: Some("src".into()),
                    expr: ScalarExpr::Column(0),
                },
                ProjectItem {
                    alias: Some("tag".into()),
                    expr: ScalarExpr::Literal(ScalarValue::Utf8("x".into())),
                },
            ],
            child: g1,
        }))
        .unwrap();
    assert_eq!(
        lineage(&memo, computed),
        [source_column("flows", "src"), ColumnLineage::Computed]
    );
    let tagged = memo
        .find_or_add_group(filter(equals(1, "x"), computed))
        .unwrap();
    let state = memo.find_or_add_group(hll(tagged, vec![], "src")).unwrap();
    let coverage = memo.groups()[state.0 as usize]
        .group_properties
        .aggregate_coverage
        .as_ref()
        .expect("a summary state");
    assert!(coverage.selection()[0].columns().is_empty());
    assert!(coverage
        .definition()
        .unlifted_operators
        .contains(&filter(equals(1, "x"), MemoGroupId(0)).map_children(|_| None)));
}

/// `Scan latency`: `[ts, value, job]`, `ts` its time index.
fn latency_series() -> Operator<MemoGroupId> {
    Operator::NonASAP(NonASAPOp::Scan {
        source: Source::TimeSeries {
            metric: "latency".into(),
        },
        predicates: vec![],
        schema: Schema::with_time_index(
            vec![
                Field::plain("ts", DataType::Timestamp, false),
                Field::plain("value", DataType::Float64, false),
                Field::plain("job", DataType::Utf8, false),
            ],
            0,
            vec![],
        ),
    })
}

/// A chain of `SummaryAgg[KLL](by job; value)` windows: `[window_end, job,
/// state]`. `window_end` (column 0) and the state are computed and the key
/// keeps its lineage. A `TimeShift` and a `TimeRange` over the chain select
/// windows, so they pass the chain's lineage through; the merge of the
/// selected windows has the layout of the chain's build, `[job, state]`.
#[test]
fn a_chain_computes_window_end_and_state_and_its_window_selections_pass_lineage_through() {
    let mut memo = memo();
    let series = memo.find_or_add_group(latency_series()).unwrap();
    let chain = memo
        .find_or_add_group(Operator::ASAP(ASAPOp::SummaryTumblingWindowChain {
            child: series,
            summary_type: FieldDataType::Sketch(SketchAlgorithm::Kll),
            input: SummaryUpdate::column(ColumnRef::SampleValue),
            reduction: Reduction::by(vec![2]),
            filter: None,
            tumbling_window_width_ms: 60_000,
        }))
        .unwrap();
    let chain_lineage = [
        ColumnLineage::Computed,
        latency_column("job"),
        ColumnLineage::Computed,
    ];
    assert_eq!(lineage(&memo, chain), chain_lineage);

    let an_hour_ago = memo
        .find_or_add_group(shift(3_600_000, None, chain))
        .unwrap();
    assert_eq!(lineage(&memo, an_hour_ago), chain_lineage);
    let five_minutes_an_hour_ago = memo.find_or_add_group(range(300_000, an_hour_ago)).unwrap();
    assert_eq!(lineage(&memo, five_minutes_an_hour_ago), chain_lineage);

    let merged = memo
        .find_or_add_group(Operator::ASAP(ASAPOp::SummaryMerge {
            children: vec![five_minutes_an_hour_ago],
            group_by: None,
        }))
        .unwrap();
    assert_eq!(
        lineage(&memo, merged),
        [latency_column("job"), ColumnLineage::Computed]
    );
}

/// The PromQL front end lowers `m[5m] @ end()` as `TimeShift { at: End }`
/// above the selector lowered without `@`: it pins when the subtree is
/// read and copies its rows, so it passes the selector's lineage through,
/// as `@ start()` does.
#[test]
fn a_time_shift_at_end_or_start_above_a_selector_passes_its_lineage_through() {
    let mut memo = memo();
    let series = memo.find_or_add_group(latency_series()).unwrap();
    let selector = memo.find_or_add_group(range(300_000, series)).unwrap();
    let series_lineage = ["ts", "value", "job"].map(latency_column);
    for at in [AtModifier::End, AtModifier::Start] {
        let pinned = memo
            .find_or_add_group(shift(0, Some(at), selector))
            .unwrap();
        assert_eq!(lineage(&memo, pinned), series_lineage);
    }
}
