//! A DAG's materialized views and their retention
//! (`asap_types::ir::materialized_view`): two one-minute KLL panes of `m`
//! kept at ingestion, merged into a view refreshed on first read.

use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;

use asap_types::ir::materialized_view::{
    InstallableDagError, MaterializedViewRefresh, RefreshWindow,
};
use asap_types::ir::operator::operator_properties::{Reduction, Source};
use asap_types::ir::operator::pane_chain::expand_pane_chains_in_workload;
use asap_types::ir::operator::TimeShift;
use asap_types::ir::physical_export::{
    compile_physical_asap_dag, compile_physical_asap_workload, PhysicalASAPDAG,
};
use asap_types::ir::scalar::ColumnRef;
use asap_types::ir::schema::{
    DataType, Field, FieldDataType, GroupingStrategy, Schema, SketchAlgorithm, SketchKind,
    SketchParams, SummaryUpdate,
};
use asap_types::ir::{
    apply_materialization_timings, apply_materialization_timings_to_roots, ASAPOp,
    MaterializationAssignment, NodeToTimedNodeCache, NonASAPOp, Operator, OperatorNode, QueryRoot,
    TimeRangeKind,
};
use asap_types::physical::{Materialization, Medium};

/// A KLL of `m`'s values over `(T - shift - 1 min, T - shift]`.
fn pane(scan: &Rc<OperatorNode>, shift_ms: i64) -> Rc<OperatorNode> {
    let shifted = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::TimeShift {
        shift: TimeShift {
            offset_ms: shift_ms,
            at: None,
        },
        child: Rc::clone(scan),
    }))
    .unwrap();
    let range = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::TimeRange {
        range: Duration::from_secs(60),
        kind: TimeRangeKind::Range,
        child: shifted,
    }))
    .unwrap();
    OperatorNode::new_shared(Operator::ASAP(ASAPOp::SummaryAgg {
        child: range,
        family: FieldDataType::Sketch(
            SketchKind::new(
                SketchAlgorithm::Kll,
                SketchParams::Kll {
                    k: 200,
                    failure_probability: None,
                },
            ),
            Default::default(),
        ),
        input: SummaryUpdate::column(ColumnRef::Named("value".into())),
        reduction: Reduction::by(vec![2]),
        grouping: GroupingStrategy::default(),
        filter: None,
    }))
    .unwrap()
}

/// `m` with columns `ts`, `value` and `job`.
fn scan_m() -> Rc<OperatorNode> {
    OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Scan {
        source: Source::TimeSeries { metric: "m".into() },
        predicates: vec![],
        schema: Schema::with_time_index(
            vec![
                Field::plain("ts", DataType::Timestamp, false),
                Field::plain("value", DataType::Float64, false),
                Field::plain("job", DataType::Utf8, true),
            ],
            0,
            vec![],
        ),
    }))
    .unwrap()
}

/// The panes at offsets 0 and 1 min kept at ingestion, their merge kept
/// across evaluations, exported: `(dag, merge's node id)`.
fn panes_and_kept_merge() -> (PhysicalASAPDAG, usize) {
    let scan = scan_m();
    let panes = [pane(&scan, 0), pane(&scan, 60_000)];
    let merge = OperatorNode::new_shared(Operator::ASAP(ASAPOp::SummaryMerge {
        children: panes.to_vec(),
        newest_panes: None,
        group_by: None,
    }))
    .unwrap();
    let mut assignment = MaterializationAssignment::default();
    for pane in &panes {
        assignment.set(pane, Materialization::MatAtIngestion(Medium::Memory));
    }
    assignment.set(
        &merge,
        Materialization::MatAtQueryKeptAcrossEvaluations(Medium::Memory),
    );
    let timed =
        apply_materialization_timings(&merge, &assignment, &mut NodeToTimedNodeCache::new())
            .unwrap();
    let dag = compile_physical_asap_dag(&timed).unwrap();
    let merge_id = dag.roots[0];
    (dag, merge_id)
}

/// A view without `retention_ms` (a DAG before installation) has no
/// materialized views to list.
#[test]
fn a_view_without_retention_is_refused() {
    let (dag, _) = panes_and_kept_merge();
    assert!(matches!(
        dag.materialized_views(),
        Err(InstallableDagError::ViewWithoutRetention { .. })
    ));
}

/// The merge, refreshed on first read for each step of a one-hour range,
/// is kept for its two minutes plus the hour; each pane it reads is kept
/// that long too, since the merge may be refreshed for its oldest step.
/// The panes read `m` at ingestion, so no raw data is kept for queries.
#[test]
fn retention_propagates_from_a_view_refreshed_on_first_read_to_its_inputs() {
    let (mut dag, merge_id) = panes_and_kept_merge();
    let spans = HashMap::from([(merge_id, 3_600_000)]);
    dag.set_retention(&HashMap::new(), &spans, None).unwrap();
    let views = dag.materialized_views().unwrap();
    let (merge, panes) = views.split_last().unwrap();
    assert_eq!(merge.node_id, merge_id);
    assert_eq!(merge.refresh(), MaterializedViewRefresh::OnFirstRead);
    assert_eq!(merge.retention_ms(), 120_000 + 3_600_000);
    assert_eq!(panes.len(), 2);
    for pane in panes {
        assert_eq!(pane.refresh(), MaterializedViewRefresh::AtIngestion);
        assert_eq!(pane.retention_ms(), merge.retention_ms());
        let node = dag.nodes.iter().find(|n| n.id == pane.node_id).unwrap();
        assert_eq!(node.retention_ms, Some(pane.retention_ms()));
    }
    assert_eq!(dag.raw_data_retention().unwrap(), []);
}

/// A view kept for less than its data reaches back (the older pane, which
/// reaches 2 minutes back, kept for 1 minute) is refused.
#[test]
fn a_view_kept_shorter_than_its_reach_is_refused() {
    let (mut dag, _) = panes_and_kept_merge();
    let views = dag
        .set_retention(&HashMap::new(), &HashMap::new(), None)
        .unwrap();
    let older = views
        .iter()
        .find(|v| {
            v.inputs
                .raw_data_windows()
                .first()
                .map(|w| w.end_offset_ms())
                == Some(60_000)
        })
        .unwrap()
        .node_id;
    dag.nodes
        .iter_mut()
        .find(|n| n.id == older)
        .unwrap()
        .retention_ms = Some(60_000);
    assert_eq!(
        dag.materialized_views(),
        Err(InstallableDagError::ViewRetentionTooShort { node_id: older })
    );
}

/// An expired deadline stops the derivation.
#[test]
fn the_derivation_stops_at_its_deadline() {
    let (mut dag, _) = panes_and_kept_merge();
    assert_eq!(
        dag.set_retention(
            &HashMap::new(),
            &HashMap::new(),
            Some(std::time::Instant::now())
        ),
        Err(InstallableDagError::DeadlineExceeded)
    );
}

/// A quantile sketch of `algorithm` (KLL k = 200 or DDSketch α = 0.01).
fn quantile_sketch(algorithm: SketchAlgorithm) -> FieldDataType {
    let params = match algorithm {
        SketchAlgorithm::Kll => SketchParams::Kll {
            k: 200,
            failure_probability: None,
        },
        _ => SketchParams::DDSketch { alpha: 0.01 },
    };
    FieldDataType::Sketch(SketchKind::new(algorithm, params), Default::default())
}

/// `pane_count` panes of `pane_width_ms` of `family` per `job` over `input`.
fn chain(
    input: &Rc<OperatorNode>,
    family: FieldDataType,
    pane_width_ms: u64,
    pane_count: u32,
) -> Rc<OperatorNode> {
    OperatorNode::new_shared(Operator::ASAP(ASAPOp::PaneChain {
        child: Rc::clone(input),
        family,
        input: SummaryUpdate::column(ColumnRef::Named("value".into())),
        reduction: Reduction::by(vec![2]),
        grouping: GroupingStrategy::default(),
        filter: None,
        pane_width_ms,
        pane_count,
    }))
    .unwrap()
}

/// Three queries over one `Scan` of `m`, each merging a chain kept at
/// ingestion: a KLL of six 10 s panes, a DDSketch of thirty 10 s panes, and
/// a KLL of five 60 s panes. The DAG Stage 3 costs (chains unexpanded) and
/// the installed DAG (chains expanded into panes, retention set).
fn three_chains_over_one_scan() -> (PhysicalASAPDAG, PhysicalASAPDAG) {
    let scan = scan_m();
    let chains = [
        chain(&scan, quantile_sketch(SketchAlgorithm::Kll), 10_000, 6),
        chain(
            &scan,
            quantile_sketch(SketchAlgorithm::DDSketch),
            10_000,
            30,
        ),
        chain(&scan, quantile_sketch(SketchAlgorithm::Kll), 60_000, 5),
    ];
    let mut assignment = MaterializationAssignment::default();
    let roots: Vec<QueryRoot> = chains
        .iter()
        .map(|chain| {
            assignment.set(chain, Materialization::MatAtIngestion(Medium::Memory));
            QueryRoot::Operator(
                OperatorNode::new_shared(Operator::ASAP(ASAPOp::SummaryMerge {
                    children: vec![Rc::clone(chain)],
                    newest_panes: None,
                    group_by: None,
                }))
                .unwrap(),
            )
        })
        .collect();
    let timed = apply_materialization_timings_to_roots(&roots, &assignment).unwrap();
    let unexpanded = compile_physical_asap_workload(&timed).unwrap();
    let expanded = expand_pane_chains_in_workload(&timed).unwrap();
    let mut installed = compile_physical_asap_workload(&expanded.roots).unwrap();
    installed
        .set_retention(&HashMap::new(), &HashMap::new(), None)
        .unwrap();
    (unexpanded, installed)
}

/// The width and sketch of the panes `node` of `dag` builds.
fn pane_shape(dag: &PhysicalASAPDAG, node: usize) -> (u64, SketchAlgorithm) {
    let payload = &dag.nodes.iter().find(|n| n.id == node).unwrap().payload;
    let (family, width_ms) = match payload {
        Operator::ASAP(ASAPOp::PaneChain {
            family,
            pane_width_ms,
            ..
        }) => (family, *pane_width_ms),
        Operator::ASAP(ASAPOp::SummaryAgg { family, child, .. }) => {
            let range = &dag.nodes.iter().find(|n| n.id == *child).unwrap().payload;
            let Operator::NonASAP(NonASAPOp::TimeRange { range, .. }) = range else {
                panic!("a pane reads a TimeRange")
            };
            (family, range.as_millis() as u64)
        }
        other => panic!("not a pane: {other:?}"),
    };
    let FieldDataType::Sketch(kind, _) = family else {
        panic!("a sketch")
    };
    (width_ms, *kind.algorithm())
}

/// A chain kept at ingestion has its newest pane's refresh window on the
/// DAG Stage 3 costs, and each of its expanded panes has the same one.
#[test]
fn a_chain_and_each_of_its_panes_have_one_refresh_window() {
    let (unexpanded, installed) = three_chains_over_one_scan();
    let window_of = |width_ms| RefreshWindow {
        width_ms,
        phase_ms: 0,
    };
    let chains: Vec<usize> = unexpanded
        .nodes
        .iter()
        .filter(|node| matches!(node.payload, Operator::ASAP(ASAPOp::PaneChain { .. })))
        .map(|node| node.id)
        .collect();
    assert_eq!(chains.len(), 3);
    for chain in chains {
        let (width_ms, _) = pane_shape(&unexpanded, chain);
        assert_eq!(unexpanded.refresh_window(chain), Some(window_of(width_ms)));
    }
    let views = installed.materialized_views().unwrap();
    assert_eq!(views.len(), 6 + 30 + 5);
    for view in &views {
        let (width_ms, _) = pane_shape(&installed, view.node_id);
        assert_eq!(
            installed.refresh_window(view.node_id),
            Some(window_of(width_ms))
        );
    }
}

/// The 10 s KLL and 10 s DDSketch panes are one refresh group, the 60 s
/// KLL panes another; the merges, at query time, are in none.
#[test]
fn panes_of_one_width_form_one_refresh_group() {
    let (_, installed) = three_chains_over_one_scan();
    let groups = installed.ingestion_refresh_groups().unwrap();
    let shapes: Vec<_> = groups
        .iter()
        .map(|group| {
            let mut shapes: Vec<_> = group
                .views
                .iter()
                .map(|view| pane_shape(&installed, view.node_id))
                .collect();
            shapes.sort_by_key(|(width_ms, algorithm)| (*width_ms, format!("{algorithm:?}")));
            shapes.dedup();
            (group.refresh_window, shapes)
        })
        .collect();
    assert_eq!(
        shapes,
        [
            (
                Some(RefreshWindow {
                    width_ms: 10_000,
                    phase_ms: 0
                }),
                vec![
                    (10_000, SketchAlgorithm::DDSketch),
                    (10_000, SketchAlgorithm::Kll)
                ]
            ),
            (
                Some(RefreshWindow {
                    width_ms: 60_000,
                    phase_ms: 0
                }),
                vec![(60_000, SketchAlgorithm::Kll)]
            ),
        ]
    );
    assert_eq!(groups[0].views.len(), 6 + 30);
    assert_eq!(groups[1].views.len(), 5);
}

/// A pane whose window ends 30 s after a multiple of its 60 s width (a
/// `TimeShift(90 s)` below its range) is refreshed on another grid than a
/// pane at no offset: another refresh group.
#[test]
fn a_shifted_pane_has_its_own_phase() {
    let scan = scan_m();
    let panes = [pane(&scan, 0), pane(&scan, 90_000)];
    let mut assignment = MaterializationAssignment::default();
    for pane in &panes {
        assignment.set(pane, Materialization::MatAtIngestion(Medium::Memory));
    }
    let roots: Vec<QueryRoot> = panes
        .iter()
        .map(|pane| QueryRoot::Operator(Rc::clone(pane)))
        .collect();
    let timed = apply_materialization_timings_to_roots(&roots, &assignment).unwrap();
    let mut dag = compile_physical_asap_workload(&timed).unwrap();
    dag.set_retention(&HashMap::new(), &HashMap::new(), None)
        .unwrap();
    let mut windows: Vec<(u64, u64)> = dag
        .ingestion_refresh_groups()
        .unwrap()
        .iter()
        .map(|group| {
            let window = group.refresh_window.unwrap();
            (window.width_ms, window.phase_ms)
        })
        .collect();
    windows.sort_unstable();
    assert_eq!(windows, [(60_000, 0), (60_000, 30_000)]);
}

/// Two views kept at ingestion over rows with no time window (`Values`)
/// have no refresh window: each is a group of its own.
#[test]
fn views_of_unbounded_windows_are_groups_of_their_own() {
    let rows = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Values {
        rows: vec![],
        schema: scan_m().schema.clone(),
    }))
    .unwrap();
    let mut assignment = MaterializationAssignment::default();
    let roots: Vec<QueryRoot> = [SketchAlgorithm::Kll, SketchAlgorithm::DDSketch]
        .into_iter()
        .map(|algorithm| {
            let build = OperatorNode::new_shared(Operator::ASAP(ASAPOp::SummaryAgg {
                child: Rc::clone(&rows),
                family: quantile_sketch(algorithm),
                input: SummaryUpdate::column(ColumnRef::Named("value".into())),
                reduction: Reduction::by(vec![2]),
                grouping: GroupingStrategy::default(),
                filter: None,
            }))
            .unwrap();
            assignment.set(&build, Materialization::MatAtIngestion(Medium::Memory));
            QueryRoot::Operator(build)
        })
        .collect();
    let timed = apply_materialization_timings_to_roots(&roots, &assignment).unwrap();
    let mut dag = compile_physical_asap_workload(&timed).unwrap();
    dag.set_retention(&HashMap::new(), &HashMap::new(), None)
        .unwrap();
    let groups = dag.ingestion_refresh_groups().unwrap();
    assert_eq!(groups.len(), 2);
    for group in &groups {
        assert_eq!(group.refresh_window, None);
        assert_eq!(group.views.len(), 1);
    }
}

/// A rollup across jobs of a pane, both kept at ingestion, reads the pane's
/// window: one refresh group, the pane listed before the rollup that reads
/// it, so a run whose roots hold both computes the pane in the run.
#[test]
fn a_view_reading_a_view_of_its_group_follows_it() {
    let scan = scan_m();
    let pane = pane(&scan, 0);
    let rollup = OperatorNode::new_shared(Operator::ASAP(ASAPOp::SummaryMerge {
        children: vec![Rc::clone(&pane)],
        newest_panes: None,
        group_by: Some(vec![]),
    }))
    .unwrap();
    let mut assignment = MaterializationAssignment::default();
    for view in [&pane, &rollup] {
        assignment.set(view, Materialization::MatAtIngestion(Medium::Memory));
    }
    let timed = apply_materialization_timings_to_roots(
        &[QueryRoot::Operator(Rc::clone(&rollup))],
        &assignment,
    )
    .unwrap();
    let mut dag = compile_physical_asap_workload(&timed).unwrap();
    dag.set_retention(&HashMap::new(), &HashMap::new(), None)
        .unwrap();
    let groups = dag.ingestion_refresh_groups().unwrap();
    assert_eq!(groups.len(), 1);
    let [pane_view, rollup_view] = groups[0].views.as_slice() else {
        panic!("{groups:#?}")
    };
    assert_eq!(rollup_view.node_id, dag.roots[0]);
    assert_eq!(rollup_view.inputs.materialized_views, [pane_view.node_id]);
    assert_eq!(
        groups[0].refresh_window,
        Some(RefreshWindow {
            width_ms: 60_000,
            phase_ms: 0
        })
    );
}

/// On the DAG Stage 3 costs (chains unexpanded), a node above a chain has
/// the window of all the chain's panes: a merge of six 10 s panes, refreshed
/// at ingestion, has refresh window (60 s, 0).
#[test]
fn a_node_above_an_unexpanded_chain_has_the_window_of_its_panes() {
    let (unexpanded, _) = three_chains_over_one_scan();
    let merge_of_kll_10s = unexpanded.roots[0];
    assert_eq!(
        unexpanded.refresh_window(merge_of_kll_10s),
        Some(RefreshWindow {
            width_ms: 60_000,
            phase_ms: 0
        })
    );
}
