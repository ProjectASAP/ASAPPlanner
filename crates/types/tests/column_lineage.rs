//! Column lineage, derived once per memo group from its input groups'
//! (concepts doc `memo-groups-properties-requirements.md`, section 3,
//! "Column lineage"): the doc's examples 1 to 3. Example 4 (PromQL label
//! matchers) is `scan_predicates_lift_and_their_states_merge` in
//! `memo_aggregate_coverage.rs`. Then a self-join's two scans, and the
//! lineage and sign of PromQL match-grouping labels.
use std::collections::BTreeMap;

use asap_types::ir::operator::arguments::{
    AtModifier, BinaryOpKind, GroupSide, JoinKind, PromQLVectorSetOpKind, Reduction, Source,
    SourceColumn, VectorGrouping, VectorMatch, VectorMatchKind,
};
use asap_types::ir::operator::AggIntent;
use asap_types::ir::properties::selection::Constraint;
use asap_types::ir::scalar::expr_ir::{ArithmeticOpKind, CompareOpKind, ScalarValue};
use asap_types::ir::scalar::ColumnRef;
use asap_types::ir::schema::{DataType, Field, FieldDataType, Schema};
use asap_types::ir::{
    ASAPOp, BinaryOperator, ExprSemantics, NonASAPOp, Operator, Predicate, ProjectItem, ScalarExpr,
};
use asap_types::stages::{
    CandidateForMemoGroup, ColumnLineage, Memo, MemoGroupId, OriginOfCandidateForMemoGroup,
    ValueSign,
};
use asap_types::summary_type::{SketchAlgorithm, SketchStatistic, SummaryUpdate};
use asap_types::test_support::memo_fixtures::{
    self, aggregate, filter, range, series_column, shift, table_column,
};
use asap_types::workload::{DataWorkload, SourceFacts};

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

/// Column `column` of memo group `group`, where it was computed.
fn computed(group: MemoGroupId, column: usize) -> ColumnLineage {
    ColumnLineage::Computed { group, column }
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
    let flows = ["src", "dst", "proto", "ts"]
        .map(|column| ColumnLineage::SourceColumn(table_column("flows", 0, column)));
    assert_eq!(lineage(&memo, g1), flows);
    assert_eq!(lineage(&memo, g2), flows);
    // Each of the aggregate, the state and the readout computes its own
    // second column.
    for group in [g3, g4, g5] {
        assert_eq!(
            lineage(&memo, group),
            [
                ColumnLineage::SourceColumn(table_column("flows", 0, "src")),
                computed(group, 1)
            ]
        );
    }
    // The state's selection names the filtered column by its lineage.
    let coverage = memo.groups()[g4.0 as usize]
        .group_properties
        .aggregate_coverage
        .as_ref()
        .expect("a summary state");
    assert_eq!(
        coverage.selection()[0].columns()[&table_column("flows", 0, "proto")],
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
    let candidate =
        CandidateForMemoGroup::new(operator, None, OriginOfCandidateForMemoGroup::Pass1, vec![]);
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
            ColumnLineage::SourceColumn(table_column("flows", 0, "src")),
            ColumnLineage::SourceColumn(table_column("flows", 0, "dst")),
            ColumnLineage::SourceColumn(table_column("flows", 0, "region")),
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
        coverage.selection()[0].columns()[&table_column("flows", 0, "region")],
        Constraint::In(vec![ScalarValue::Utf8("us".into())])
    );

    let rolled_up_id = memo
        .find_or_add_group(Operator::ASAP(ASAPOp::SummaryMerge {
            children: vec![by_source_and_area],
            group_by: Some(vec![0]),
        }))
        .unwrap();
    let rolled_up = &memo.groups()[rolled_up_id.0 as usize].group_properties;
    // The regrouped state is computed by the merge, not its input's.
    assert_eq!(
        rolled_up.column_lineage,
        [
            ColumnLineage::SourceColumn(table_column("flows", 0, "src")),
            computed(rolled_up_id, 1)
        ]
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
            ColumnLineage::SourceColumn(table_column("sales", 0, "user")),
            ColumnLineage::SourceColumn(table_column("sales", 0, "region")),
            ColumnLineage::SourceColumn(table_column("users", 0, "id")),
            ColumnLineage::SourceColumn(table_column("users", 0, "region")),
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
        [table_column("sales", 0, "region")]
    );
    assert_eq!(
        selected_column(&mut memo, 3),
        [table_column("users", 0, "region")]
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
    let tagged_rows = memo
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
        lineage(&memo, tagged_rows),
        [
            ColumnLineage::SourceColumn(table_column("flows", 0, "src")),
            computed(tagged_rows, 1)
        ]
    );
    let tagged = memo
        .find_or_add_group(filter(equals(1, "x"), tagged_rows))
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
    memo_fixtures::scan("latency", &[("job", DataType::Utf8)], vec![])
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
        computed(chain, 0),
        ColumnLineage::SourceColumn(series_column("latency", 0, "job")),
        computed(chain, 2),
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
        [
            ColumnLineage::SourceColumn(series_column("latency", 0, "job")),
            computed(merged, 1)
        ]
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
    let series_lineage = ["ts", "value", "job"]
        .map(|column| ColumnLineage::SourceColumn(series_column("latency", 0, column)));
    for at in [AtModifier::End, AtModifier::Start] {
        let pinned = memo
            .find_or_add_group(shift(0, Some(at), selector))
            .unwrap();
        assert_eq!(lineage(&memo, pinned), series_lineage);
    }
}

/// `SELECT approx_distinct(<p>_dst) FROM (SELECT a.src AS <p>_src, a.proto
/// AS <p>_proto, b.dst AS <p>_dst, b.proto AS <p>_other_proto FROM flows a
/// JOIN flows b ON a.src = b.src WHERE <side>.proto = 'tcp')`, aliases
/// prefixed by `prefix`, filtering the left side's `proto` (column 2 of the
/// join) or the right side's (column 5): the HLL state's selection keys.
fn self_join_selection(memo: &mut Memo, prefix: &str, proto_column: usize) -> Vec<SourceColumn> {
    let flows = memo
        .find_or_add_group(scan("flows", &["src", "dst", "proto"]))
        .unwrap();
    let joined = memo
        .find_or_add_group(Operator::NonASAP(NonASAPOp::Join {
            kind: JoinKind::Inner,
            pred: Predicate(ScalarExpr::Compare {
                left: Box::new(ScalarExpr::Column(0)),
                op: CompareOpKind::Eq,
                right: Box::new(ScalarExpr::Column(3)),
                semantics: ExprSemantics::Sql,
            }),
            left: flows,
            right: flows,
        }))
        .unwrap();
    let tcp = memo
        .find_or_add_group(filter(equals(proto_column, "tcp"), joined))
        .unwrap();
    let renamed = memo
        .find_or_add_group(Operator::NonASAP(NonASAPOp::Project {
            cols: [("src", 0), ("proto", 2), ("dst", 4), ("other_proto", 5)]
                .map(|(alias, column)| ProjectItem {
                    alias: Some(format!("{prefix}_{alias}")),
                    expr: ScalarExpr::Column(column),
                })
                .to_vec(),
            child: tcp,
        }))
        .unwrap();
    let state = memo
        .find_or_add_group(hll(renamed, vec![], &format!("{prefix}_dst")))
        .unwrap();
    let coverage = memo.groups()[state.0 as usize]
        .group_properties
        .aggregate_coverage
        .as_ref()
        .expect("a summary state");
    let [selection] = coverage.selection() else {
        panic!("one box: {coverage:?}")
    };
    selection.columns().keys().cloned().collect()
}

/// A self-join's two scans of one source have different scan ordinals: the
/// left side's columns are scan 0's, the right side's scan 1's. So a
/// predicate on the left side's `proto` lifts keyed by `flows` scan 0
/// `proto`, one on the right side's by scan 1, and renaming the columns
/// (other aliases) keeps the key.
#[test]
fn a_self_join_names_its_two_scans_apart() {
    let mut memo = memo();
    let flows = memo
        .find_or_add_group(scan("flows", &["src", "dst", "proto"]))
        .unwrap();
    let joined = memo
        .find_or_add_group(Operator::NonASAP(NonASAPOp::Join {
            kind: JoinKind::Inner,
            pred: Predicate(ScalarExpr::Compare {
                left: Box::new(ScalarExpr::Column(0)),
                op: CompareOpKind::Eq,
                right: Box::new(ScalarExpr::Column(3)),
                semantics: ExprSemantics::Sql,
            }),
            left: flows,
            right: flows,
        }))
        .unwrap();
    assert_eq!(
        lineage(&memo, joined),
        [
            (0, "src"),
            (0, "dst"),
            (0, "proto"),
            (1, "src"),
            (1, "dst"),
            (1, "proto")
        ]
        .map(|(scan, column)| ColumnLineage::SourceColumn(table_column("flows", scan, column)))
    );

    let left_proto = [table_column("flows", 0, "proto")];
    assert_eq!(self_join_selection(&mut memo, "a", 2), left_proto);
    assert_eq!(self_join_selection(&mut memo, "x", 2), left_proto);
    assert_eq!(
        self_join_selection(&mut memo, "a", 5),
        [table_column("flows", 1, "proto")]
    );
}

/// The memo of the match-grouping examples: `team` declared non-negative
/// in `a` and non-positive in `b`.
fn match_grouping_memo() -> Memo {
    let declared = |sign| SourceFacts {
        column_signs: BTreeMap::from([("team".to_string(), sign)]),
        ..SourceFacts::default()
    };
    Memo::new(&DataWorkload {
        sources: BTreeMap::from([
            ("a".to_string(), declared(ValueSign::NonNegative)),
            ("b".to_string(), declared(ValueSign::NonPositive)),
        ]),
        ..DataWorkload::default()
    })
}

/// `Scan metric`: `[ts, value, *labels]`, `ts` its time index.
fn series(metric: &str, labels: &[&str]) -> Operator<MemoGroupId> {
    let labels: Vec<(&str, DataType)> = labels
        .iter()
        .map(|&label| (label, DataType::Utf8))
        .collect();
    memo_fixtures::scan(metric, &labels, vec![])
}

/// `lhs * on(job) group_<side>(team) rhs`.
fn grouped_product(side: GroupSide, lhs: MemoGroupId, rhs: MemoGroupId) -> Operator<MemoGroupId> {
    Operator::NonASAP(NonASAPOp::BinaryOp {
        operator: BinaryOperator {
            checked_relative_division: false,
            checked_finite_division: false,
            kind: BinaryOpKind::Arithmetic(ArithmeticOpKind::Mul),
            vector_match: Some(VectorMatch {
                kind: VectorMatchKind::On,
                labels: vec!["job".into()],
                grouping: Some(VectorGrouping {
                    side,
                    labels: vec!["team".into()],
                }),
            }),
        },
        return_bool: false,
        lhs,
        rhs,
    })
}

/// The lineage and sign of column `name` of `group`.
fn lineage_and_sign_of(memo: &Memo, group: MemoGroupId, name: &str) -> (ColumnLineage, ValueSign) {
    let properties = &memo.groups()[group.0 as usize].group_properties;
    let column = properties
        .schema
        .column_id(name)
        .expect("a column of that name");
    (
        properties.column_lineage[column].clone(),
        properties.column_signs[column],
    )
}

/// `a * on(job) group_left(team) b`: `team`, which only `b` has, takes
/// `b.team`'s lineage and sign (the "one" side's).
#[test]
fn a_group_left_label_comes_from_the_right_side() {
    let mut memo = match_grouping_memo();
    let a = memo.find_or_add_group(series("a", &["job"])).unwrap();
    let b = memo
        .find_or_add_group(series("b", &["job", "team"]))
        .unwrap();
    let product = memo
        .find_or_add_group(grouped_product(GroupSide::Left, a, b))
        .unwrap();
    assert_eq!(
        lineage_and_sign_of(&memo, product, "team"),
        (
            ColumnLineage::SourceColumn(series_column("b", 0, "team")),
            ValueSign::NonPositive
        )
    );
}

/// `a * on(job) group_left(team) b` with `a` having its own `team`: PromQL
/// overwrites it with `b`'s, so the column takes `b.team`'s lineage and
/// sign in place.
#[test]
fn a_group_left_label_overwrites_the_left_sides_column_of_that_name() {
    let mut memo = match_grouping_memo();
    let a = memo
        .find_or_add_group(series("a", &["job", "team"]))
        .unwrap();
    let b = memo
        .find_or_add_group(series("b", &["job", "team"]))
        .unwrap();
    let product = memo
        .find_or_add_group(grouped_product(GroupSide::Left, a, b))
        .unwrap();
    let schema = &memo.groups()[product.0 as usize].group_properties.schema;
    assert_eq!(schema.fields.len(), 4, "no second `team` column");
    assert_eq!(
        lineage_and_sign_of(&memo, product, "team"),
        (
            ColumnLineage::SourceColumn(series_column("b", 0, "team")),
            ValueSign::NonPositive
        )
    );
}

/// `a * on(job) group_right(team) b`: `team` takes `a.team`'s lineage and
/// sign (the left, "one" side's).
#[test]
fn a_group_right_label_comes_from_the_left_side() {
    let mut memo = match_grouping_memo();
    let a = memo
        .find_or_add_group(series("a", &["job", "team"]))
        .unwrap();
    let b = memo
        .find_or_add_group(series("b", &["job", "team"]))
        .unwrap();
    let product = memo
        .find_or_add_group(grouped_product(GroupSide::Right, a, b))
        .unwrap();
    assert_eq!(
        lineage_and_sign_of(&memo, product, "team"),
        (
            ColumnLineage::SourceColumn(series_column("a", 0, "team")),
            ValueSign::NonNegative
        )
    );
}

/// `rate(x[<width>])`: per series, `[ts, value, job, team]` kept in place.
fn rate_of_x(memo: &mut Memo, width_ms: i64) -> MemoGroupId {
    let x = memo
        .find_or_add_group(series("x", &["job", "team"]))
        .unwrap();
    let window = memo.find_or_add_group(range(width_ms, x)).unwrap();
    memo.find_or_add_group(aggregate(Reduction::PerEntity, AggIntent::Rate, window))
        .unwrap()
}

/// `rate(x[5m]) / rate(x[1h])`: both sides read `x`, but a `BinaryOp`
/// does not concatenate their columns; its labels are the left side's, so
/// `job` is still `x` scan 0's.
#[test]
fn a_same_source_binary_op_keeps_its_labels_scan() {
    let mut memo = match_grouping_memo();
    let short = rate_of_x(&mut memo, 300_000);
    let long = rate_of_x(&mut memo, 3_600_000);
    let ratio = memo
        .find_or_add_group(Operator::NonASAP(NonASAPOp::BinaryOp {
            operator: BinaryOperator {
                checked_relative_division: false,
                checked_finite_division: false,
                kind: BinaryOpKind::Arithmetic(ArithmeticOpKind::Div),
                vector_match: None,
            },
            return_bool: false,
            lhs: short,
            rhs: long,
        }))
        .unwrap();
    assert_eq!(
        lineage_and_sign_of(&memo, ratio, "job").0,
        ColumnLineage::SourceColumn(series_column("x", 0, "job"))
    );
}

/// `rate(x[5m]) * on(job) group_right(team) rate(x[1h])`: the output's rows
/// are the right ("many") side's, so its labels take that side's lineage,
/// `x` scan 0's, not a shifted scan or a disagreement; `team` comes from
/// the left ("one") side, also `x` scan 0's.
#[test]
fn a_same_source_group_right_takes_the_right_sides_labels() {
    let mut memo = match_grouping_memo();
    let short = rate_of_x(&mut memo, 300_000);
    let long = rate_of_x(&mut memo, 3_600_000);
    let product = memo
        .find_or_add_group(grouped_product(GroupSide::Right, short, long))
        .unwrap();
    for label in ["job", "team"] {
        assert_eq!(
            lineage_and_sign_of(&memo, product, label).0,
            ColumnLineage::SourceColumn(series_column("x", 0, label))
        );
    }
}

/// `a or b` with only `b` having `team`: on `a`'s rows `team` is null, so it
/// is computed by the `or` with no sign, not `b.team` (before, it copied
/// `b.team` and `b`'s sign).
#[test]
fn an_or_label_only_the_right_side_has_is_computed() {
    let mut memo = match_grouping_memo();
    let a = memo.find_or_add_group(series("a", &["job"])).unwrap();
    let b = memo
        .find_or_add_group(series("b", &["job", "team"]))
        .unwrap();
    let union = memo
        .find_or_add_group(Operator::NonASAP(NonASAPOp::BinaryOp {
            operator: BinaryOperator {
                checked_relative_division: false,
                checked_finite_division: false,
                kind: BinaryOpKind::Set(PromQLVectorSetOpKind::Or),
                vector_match: None,
            },
            return_bool: false,
            lhs: a,
            rhs: b,
        }))
        .unwrap();
    let team = memo.groups()[union.0 as usize]
        .group_properties
        .schema
        .column_id("team")
        .expect("`or` adds the right side's labels");
    assert_eq!(
        lineage_and_sign_of(&memo, union, "team"),
        (computed(union, team), ValueSign::Unknown)
    );
}
