//! Wider tumbling-window levels built from a narrower chain
//! (`SummaryMergeWindows`) and a `TimeRangeCover` over the levels run
//! natively (design doc `multi-level-window-chains.md`): each ingestion run
//! of a wider level merges the narrower windows inside its window, and a
//! query run merges the whole wider windows inside its range with the
//! narrower windows at the edges, at evaluation times on and off the wider
//! grid.
use asap_executor::physical_planner::{compile, test_support::views_at};
use asap_executor::runtime::{Limits, RunContext, Scope};
use asap_executor::sources::InMemoryDataStore;
use asap_executor::values::{Batch, Value};
use planner_types::ir::operator::{Reduction, Source};
use planner_types::ir::scalar::ColumnRef;
use planner_types::ir::schema::{DataType, Field, FieldDataType, Schema};
use planner_types::ir::{ASAPOp, NonASAPOp, Operator, OperatorNode, QueryRoot, TimeRangeKind};
use planner_types::plan::materialized_view::MaterializedView;
use planner_types::plan::physical_asap_dag::{PhysicalASAPDAG, PhysicalASAPNodeId};
use planner_types::summary_type::{
    ExactKind, ExactParams, SketchAlgorithm, SketchParams, SketchStatistic, SummaryUpdate,
};
use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

const SECOND_MS: i64 = 1_000;
const NARROW_MS: i64 = 15 * SECOND_MS;
const WIDE_MS: i64 = 300 * SECOND_MS;
const HOUR_MS: i64 = 3_600 * SECOND_MS;

fn samples_schema() -> Schema {
    Schema::with_time_index(
        vec![
            Field::plain("job", DataType::Utf8, false),
            Field::plain("ts", DataType::Timestamp, false),
            Field::plain("value", DataType::Float64, false),
        ],
        1,
        vec![],
    )
}

fn source() -> Source {
    Source::TimeSeries {
        metric: "latency".into(),
    }
}

fn node(operator: Operator) -> Rc<OperatorNode> {
    OperatorNode::new_shared(operator).unwrap()
}

/// One sample per job every 5 s over 3 h, values varying with time.
fn samples() -> Vec<(&'static str, i64, f64)> {
    (1..=3 * 720)
        .flat_map(|k| {
            let t = k * 5 * SECOND_MS;
            [("a", t, (k % 97) as f64), ("b", t, 2.0 * (k % 89) as f64)]
        })
        .collect()
}

/// The sum per job of the samples in `(start, end]`, computed directly.
fn direct_sums(start: i64, end: i64) -> BTreeMap<String, f64> {
    let mut sums = BTreeMap::new();
    for (job, t, value) in samples() {
        if start < t && t <= end {
            *sums.entry(job.to_string()).or_insert(0.0) += value;
        }
    }
    sums
}

fn store_with_samples() -> InMemoryDataStore {
    let schema = Arc::new(samples_schema());
    let rows = samples()
        .into_iter()
        .map(|(job, t, value)| {
            vec![
                Value::Utf8(job.into()),
                Value::Timestamp(t),
                Value::Float64(value),
            ]
        })
        .collect();
    let batch = Batch::try_new(schema.clone(), rows).unwrap();
    InMemoryDataStore::default()
        .with_raw_data(source(), schema, vec![batch])
        .unwrap()
}

fn sums_of(rows: Vec<Vec<Value>>) -> BTreeMap<String, f64> {
    rows.into_iter()
        .map(|row| {
            let job = row.iter().find_map(|value| match value {
                Value::Utf8(job) => Some(job.to_string()),
                _ => None,
            });
            let sum = row.iter().find_map(|value| match value {
                Value::Float64(sum) => Some(*sum),
                _ => None,
            });
            (job.unwrap(), sum.unwrap())
        })
        .collect()
}

fn run(
    dag: &asap_executor::plan::PhysicalDAG<'static, Batch, asap_executor::values::SchemaRef>,
    roots: &[u64],
    context: RunContext,
) -> Vec<Vec<Vec<Value>>> {
    crate::common::run_roots(dag, roots, context)
        .unwrap()
        .into_iter()
        .map(|batches| batches.iter().flat_map(Batch::to_rows).collect())
        .collect()
}

fn id_of(nodes: &[Rc<OperatorNode>], operator: &Rc<OperatorNode>) -> PhysicalASAPNodeId {
    nodes
        .iter()
        .position(|node| Rc::ptr_eq(node, operator))
        .unwrap()
}

/// What the plan builds and reads: exact sums, or DDSketch states read
/// out as their p99.
#[derive(Clone, Copy)]
enum Built {
    Sum,
    Max,
    DDSketchP99,
    KllP99,
}

fn chain(width_ms: i64, built: Built) -> Rc<OperatorNode> {
    node(Operator::ASAP(ASAPOp::SummaryTumblingWindowChain {
        child: node(Operator::NonASAP(NonASAPOp::Scan {
            source: source(),
            predicates: vec![],
            schema: samples_schema(),
        })),
        summary_type: match built {
            Built::Sum => FieldDataType::ExactAggregate(ExactKind::Sum, ExactParams::Sum),
            Built::Max => FieldDataType::ExactAggregate(ExactKind::Max, ExactParams::Max),
            Built::DDSketchP99 => FieldDataType::Sketch(SketchAlgorithm::DDSketch),
            Built::KllP99 => FieldDataType::Sketch(SketchAlgorithm::Kll),
        },
        input: SummaryUpdate::column(ColumnRef::SampleValue),
        reduction: Reduction::by(vec![0]),
        filter: None,
        tumbling_window_width_ms: width_ms as u64,
    }))
}

/// The merge of `windows`, read out: finalized sums, or each p99.
fn finalized_merge(windows: Rc<OperatorNode>, built: Built) -> Rc<OperatorNode> {
    let merged = node(Operator::ASAP(ASAPOp::SummaryMerge {
        children: vec![windows],
        group_by: None,
    }));
    node(Operator::ASAP(match built {
        Built::Sum | Built::Max => ASAPOp::FinalizeExactAccumulator {
            counter_readout: None,
            child: merged,
        },
        Built::DDSketchP99 | Built::KllP99 => ASAPOp::SummaryEstimate {
            summary_input: merged,
            query: SketchStatistic::Quantile { q: 0.99 },
        },
    }))
}

/// The θ of `built`'s sketches; `None` for exact states.
fn sketch_parameters(built: Built) -> Option<SketchParams> {
    match built {
        Built::Sum | Built::Max => None,
        Built::DDSketchP99 => Some(SketchParams::DDSketch { alpha: 0.01 }),
        Built::KllP99 => Some(SketchParams::Kll { k: 200 }),
    }
}

/// The plan: a 15 s chain of exact sums per job, a 5 min level merged from
/// it, both kept 2 h; `cover` sums `(T - 1h, T]` over the two levels,
/// `flat` over the 15 s chain alone; `direct_wide` is a 5 min chain built
/// from the rows, for comparing states.
struct Plan {
    dag: PhysicalASAPDAG,
    narrow: PhysicalASAPNodeId,
    wide: PhysicalASAPNodeId,
    direct_wide: PhysicalASAPNodeId,
    cover: PhysicalASAPNodeId,
    flat: PhysicalASAPNodeId,
    /// The newest 5 min window's sums, of `wide` and of `direct_wide`.
    newest_wide: [PhysicalASAPNodeId; 2],
    views: Vec<MaterializedView>,
}

fn plan(built: Built) -> Plan {
    let finalized_merge = |windows| finalized_merge(windows, built);
    let narrow = chain(NARROW_MS, built);
    let wide = node(Operator::ASAP(ASAPOp::SummaryMergeWindows {
        child: narrow.clone(),
        tumbling_window_width_ms: WIDE_MS as u64,
    }));
    let direct_wide = chain(WIDE_MS, built);
    let cover = finalized_merge(node(Operator::NonASAP(NonASAPOp::TimeRangeCover {
        range: Duration::from_millis(HOUR_MS as u64),
        levels: vec![wide.clone(), narrow.clone()],
    })));
    let flat = finalized_merge(node(Operator::NonASAP(NonASAPOp::TimeRange {
        range: Duration::from_millis(HOUR_MS as u64),
        kind: TimeRangeKind::Range,
        child: narrow.clone(),
    })));
    let direct = finalized_merge(node(Operator::NonASAP(NonASAPOp::TimeRange {
        range: Duration::from_millis(HOUR_MS as u64),
        kind: TimeRangeKind::Range,
        child: direct_wide.clone(),
    })));
    let newest = |level: &Rc<OperatorNode>| {
        finalized_merge(node(Operator::NonASAP(NonASAPOp::TimeRange {
            range: Duration::from_millis(WIDE_MS as u64),
            kind: TimeRangeKind::Range,
            child: level.clone(),
        })))
    };
    let newest_wide = [newest(&wide), newest(&direct_wide)];
    let (mut dag, nodes) =
        planner_types::test_support::forwarders::physical_asap_dag_at_query_time_with_node_ids(&[
            QueryRoot::Operator(cover.clone()),
            QueryRoot::Operator(flat.clone()),
            QueryRoot::Operator(direct),
            QueryRoot::Operator(newest_wide[0].clone()),
            QueryRoot::Operator(newest_wide[1].clone()),
        ])
        .unwrap();
    let ids = [
        id_of(&nodes, &narrow),
        id_of(&nodes, &wide),
        id_of(&nodes, &direct_wide),
    ];
    for node in dag.nodes.iter_mut().filter(|node| ids.contains(&node.id)) {
        node.retention_ms = Some(2 * HOUR_MS as u64);
    }
    // The sketches' θ, on every build and merge of windows.
    for node in dag.nodes.iter_mut().filter(|node| ids.contains(&node.id)) {
        node.parameters = sketch_parameters(built);
    }
    let views = views_at(&mut dag, &ids.map(|id| id as u64)).unwrap();
    Plan {
        narrow: ids[0],
        wide: ids[1],
        direct_wide: ids[2],
        cover: id_of(&nodes, &cover),
        flat: id_of(&nodes, &flat),
        newest_wide: [
            id_of(&nodes, &newest_wide[0]),
            id_of(&nodes, &newest_wide[1]),
        ],
        dag,
        views,
    }
}

/// Refreshes the views as a runtime does, in time order up to `until`:
/// the 15 s chain at the end of each of its windows, then, at the end of
/// each 5 min window, the 5 min level (reading the 15 s chain's stored
/// windows) and the 5 min chain built from the rows, each run's rows
/// appended to `store`. The 5 min level's rows of each of its runs.
fn refresh_all(
    plan: &Plan,
    store: &mut Arc<InMemoryDataStore>,
    until: i64,
) -> Vec<(i64, Vec<Vec<Value>>)> {
    let view = |node| {
        plan.views
            .iter()
            .find(|view| view.node_id == node)
            .unwrap()
            .clone()
    };
    let schema = |node| {
        Arc::new(
            plan.dag
                .nodes
                .iter()
                .find(|n| n.id == node)
                .unwrap()
                .output_schema
                .clone(),
        )
    };
    let compiled = [
        (
            plan.narrow,
            NARROW_MS,
            compile(&plan.dag, &[], &[plan.narrow as u64]).unwrap(),
        ),
        (
            plan.wide,
            WIDE_MS,
            compile(&plan.dag, &[view(plan.narrow)], &[plan.wide as u64]).unwrap(),
        ),
        (
            plan.direct_wide,
            WIDE_MS,
            compile(&plan.dag, &[], &[plan.direct_wide as u64]).unwrap(),
        ),
    ];
    let mut wide_runs = Vec::new();
    let mut end = NARROW_MS;
    while end <= until {
        for (node, width_ms, refresh) in &compiled {
            if end % width_ms != 0 {
                continue;
            }
            let context = RunContext::new(
                Scope::Ingestion {
                    window_start_ms: end - width_ms,
                    window_end_ms: end,
                },
                Limits::default(),
            )
            .unwrap();
            let rows = {
                let dag = refresh.instantiate(store.clone()).unwrap();
                run(&dag, &[*node as u64], context).remove(0)
            };
            let batch = Batch::try_new(schema(*node), rows.clone()).unwrap();
            Arc::get_mut(store)
                .unwrap()
                .append_materialized_view(&view(*node), (end - width_ms, end), vec![batch])
                .unwrap();
            if *node == plan.wide {
                wide_runs.push((end, rows));
            }
        }
        end += NARROW_MS;
    }
    wide_runs
}

/// T9. Each 5 min window `SummaryMergeWindows` merges from the 15 s
/// windows inside it holds what the 5 min window a chain builds from the
/// rows holds: one row per job stamped with the window's end, and the same
/// sum per job read back.
#[test]
fn merge_windows_equals_direct_build() {
    let plan = plan(Built::Sum);
    let mut store = Arc::new(store_with_samples());
    let view = |node| {
        plan.views
            .iter()
            .find(|view| view.node_id == node)
            .unwrap()
            .clone()
    };
    let merged = refresh_all(&plan, &mut store, 3 * HOUR_MS);
    assert_eq!(merged.len(), 36);
    for (end, rows) in &merged {
        assert_eq!(rows.len(), 2);
        assert!(rows
            .iter()
            .all(|row| matches!(row[0], Value::Timestamp(t) if t == *end)));
    }
    let views = [view(plan.wide), view(plan.direct_wide)];
    let roots = plan.newest_wide.map(|id| id as u64);
    let query = compile(&plan.dag, &views, &roots).unwrap();
    // The windows the 2 h retention still keeps at 3 h.
    for (end, _) in merged.into_iter().filter(|(end, _)| *end > HOUR_MS) {
        let context = RunContext::new(
            Scope::Query {
                evaluation_time_ms: end,
            },
            Limits::default(),
        )
        .unwrap();
        let answers = {
            let query = query.instantiate(store.clone()).unwrap();
            run(&query, &roots, context)
        };
        assert_eq!(sums_of(answers[0].clone()), direct_sums(end - WIDE_MS, end));
        assert_eq!(sums_of(answers[1].clone()), direct_sums(end - WIDE_MS, end));
    }
}

/// T10. The cover's answer equals the 15 s chain's and the sums computed
/// directly over the raw samples, at evaluation times on the 5 min grid
/// and off it (every 15 s over 5 minutes).
#[test]
fn cover_answers_equal_flat_answers() {
    let plan = plan(Built::Sum);
    let mut store = Arc::new(store_with_samples());
    let view = |node| {
        plan.views
            .iter()
            .find(|view| view.node_id == node)
            .unwrap()
            .clone()
    };
    refresh_all(&plan, &mut store, 3 * HOUR_MS);
    let views = [view(plan.narrow), view(plan.wide)];
    let query = compile(&plan.dag, &views, &[plan.cover as u64, plan.flat as u64]).unwrap();
    let mut t = 3 * HOUR_MS - WIDE_MS;
    while t <= 3 * HOUR_MS {
        let context = RunContext::new(
            Scope::Query {
                evaluation_time_ms: t,
            },
            Limits::default(),
        )
        .unwrap();
        let answers = {
            let query = query.instantiate(store.clone()).unwrap();
            run(&query, &[plan.cover as u64, plan.flat as u64], context)
        };
        let expected = direct_sums(t - HOUR_MS, t);
        assert_eq!(sums_of(answers[0].clone()), expected, "cover at {t}");
        assert_eq!(sums_of(answers[1].clone()), expected, "flat at {t}");
        t += NARROW_MS;
    }
}

/// T10, sketches. Over DDSketch states (merges are exact: the merged state
/// is the state built over the union), the p99 the cover reads equals the
/// p99 the 15 s chain reads, bit for bit, at every 15 s over 5 minutes.
#[test]
fn cover_quantiles_equal_flat_quantiles() {
    let plan = plan(Built::DDSketchP99);
    let mut store = Arc::new(store_with_samples());
    let view = |node| {
        plan.views
            .iter()
            .find(|view| view.node_id == node)
            .unwrap()
            .clone()
    };
    refresh_all(&plan, &mut store, 3 * HOUR_MS);
    let views = [view(plan.narrow), view(plan.wide)];
    let query = compile(&plan.dag, &views, &[plan.cover as u64, plan.flat as u64]).unwrap();
    let mut t = 3 * HOUR_MS - WIDE_MS;
    while t <= 3 * HOUR_MS {
        let context = RunContext::new(
            Scope::Query {
                evaluation_time_ms: t,
            },
            Limits::default(),
        )
        .unwrap();
        let answers = {
            let query = query.instantiate(store.clone()).unwrap();
            run(&query, &[plan.cover as u64, plan.flat as u64], context)
        };
        let (cover, flat) = (sums_of(answers[0].clone()), sums_of(answers[1].clone()));
        assert_eq!(cover.len(), 2, "at {t}");
        assert_eq!(cover, flat, "at {t}");
        t += NARROW_MS;
    }
}

/// A chain of `level_widths_ms[last]` over the rows and a level of each
/// wider width (widest first) merged from the next narrower one, all kept
/// 2 h; `cover` reads `(T - 1h, T]` over every level, `flat` over the
/// narrowest chain alone.
struct Levels {
    dag: PhysicalASAPDAG,
    /// Narrowest first, with its width.
    levels: Vec<(PhysicalASAPNodeId, i64)>,
    cover: PhysicalASAPNodeId,
    flat: PhysicalASAPNodeId,
    views: Vec<MaterializedView>,
}

fn levels_plan(built: Built, level_widths_ms: &[i64]) -> Levels {
    let finalized_merge = |windows| finalized_merge(windows, built);
    let mut widths = level_widths_ms.to_vec();
    widths.reverse();
    let mut nodes_narrowest_first = vec![chain(widths[0], built)];
    for &width in &widths[1..] {
        let narrower = nodes_narrowest_first.last().unwrap().clone();
        nodes_narrowest_first.push(node(Operator::ASAP(ASAPOp::SummaryMergeWindows {
            child: narrower,
            tumbling_window_width_ms: width as u64,
        })));
    }
    let cover = finalized_merge(node(Operator::NonASAP(NonASAPOp::TimeRangeCover {
        range: Duration::from_millis(HOUR_MS as u64),
        levels: nodes_narrowest_first.iter().rev().cloned().collect(),
    })));
    let flat = finalized_merge(node(Operator::NonASAP(NonASAPOp::TimeRange {
        range: Duration::from_millis(HOUR_MS as u64),
        kind: TimeRangeKind::Range,
        child: nodes_narrowest_first[0].clone(),
    })));
    let (mut dag, nodes) =
        planner_types::test_support::forwarders::physical_asap_dag_at_query_time_with_node_ids(&[
            QueryRoot::Operator(cover.clone()),
            QueryRoot::Operator(flat.clone()),
        ])
        .unwrap();
    let levels: Vec<(PhysicalASAPNodeId, i64)> = nodes_narrowest_first
        .iter()
        .zip(&widths)
        .map(|(level, &width)| (id_of(&nodes, level), width))
        .collect();
    for node in dag
        .nodes
        .iter_mut()
        .filter(|node| levels.iter().any(|(id, _)| *id == node.id))
    {
        node.retention_ms = Some(2 * HOUR_MS as u64);
        node.parameters = sketch_parameters(built);
    }
    let ids: Vec<u64> = levels.iter().map(|(id, _)| *id as u64).collect();
    let views = views_at(&mut dag, &ids).unwrap();
    Levels {
        cover: id_of(&nodes, &cover),
        flat: id_of(&nodes, &flat),
        dag,
        levels,
        views,
    }
}

/// Refreshes every level in time order up to `until`, as a runtime does:
/// at each window end, the narrowest chain from the rows, then each wider
/// level from the next narrower one's stored windows.
fn refresh_levels(plan: &Levels, store: &mut Arc<InMemoryDataStore>, until: i64) {
    let view = |node| {
        plan.views
            .iter()
            .find(|view| view.node_id == node)
            .unwrap()
            .clone()
    };
    let compiled: Vec<_> = plan
        .levels
        .iter()
        .enumerate()
        .map(|(index, &(id, width))| {
            let bound: Vec<_> = match index {
                0 => vec![],
                _ => vec![view(plan.levels[index - 1].0)],
            };
            (id, width, compile(&plan.dag, &bound, &[id as u64]).unwrap())
        })
        .collect();
    let narrowest = plan.levels[0].1;
    let mut end = narrowest;
    while end <= until {
        for (id, width, refresh) in &compiled {
            if end % width != 0 {
                continue;
            }
            let context = RunContext::new(
                Scope::Ingestion {
                    window_start_ms: end - width,
                    window_end_ms: end,
                },
                Limits::default(),
            )
            .unwrap();
            let rows = {
                let dag = refresh.instantiate(store.clone()).unwrap();
                run(&dag, &[*id as u64], context).remove(0)
            };
            let schema = Arc::new(
                plan.dag
                    .nodes
                    .iter()
                    .find(|n| n.id == *id)
                    .unwrap()
                    .output_schema
                    .clone(),
            );
            let batch = Batch::try_new(schema, rows).unwrap();
            Arc::get_mut(store)
                .unwrap()
                .append_materialized_view(&view(*id), (end - width, end), vec![batch])
                .unwrap();
        }
        end += narrowest;
    }
}

/// Each step's answers of `cover` and `flat`, every 15 s over the last
/// 5 minutes of 3 h, after refreshing the levels.
fn answers_by_step(plan: &Levels) -> Vec<(i64, [BTreeMap<String, f64>; 2])> {
    let mut store = Arc::new(store_with_samples());
    refresh_levels(plan, &mut store, 3 * HOUR_MS);
    let query = compile(
        &plan.dag,
        &plan.views,
        &[plan.cover as u64, plan.flat as u64],
    )
    .unwrap();
    let mut answers = Vec::new();
    let mut t = 3 * HOUR_MS - WIDE_MS;
    while t <= 3 * HOUR_MS {
        let context = RunContext::new(
            Scope::Query {
                evaluation_time_ms: t,
            },
            Limits::default(),
        )
        .unwrap();
        let rows = {
            let query = query.instantiate(store.clone()).unwrap();
            run(&query, &[plan.cover as u64, plan.flat as u64], context)
        };
        answers.push((t, [sums_of(rows[0].clone()), sums_of(rows[1].clone())]));
        t += NARROW_MS;
    }
    answers
}

/// T10, `max_over_time`. The cover's max per job equals the max computed
/// directly over the raw samples at every step.
#[test]
fn cover_maxima_equal_direct_maxima() {
    let plan = levels_plan(Built::Max, &[WIDE_MS, NARROW_MS]);
    for (t, [cover, flat]) in answers_by_step(&plan) {
        let mut expected: BTreeMap<String, f64> = BTreeMap::new();
        for (job, sample_t, value) in samples() {
            if t - HOUR_MS < sample_t && sample_t <= t {
                let max = expected.entry(job.to_string()).or_insert(f64::MIN);
                *max = max.max(value);
            }
        }
        assert_eq!(cover, expected, "cover at {t}");
        assert_eq!(flat, expected, "flat at {t}");
    }
}

/// T10, KLL. A KLL merged through the cover is not bit-identical to the
/// single chain's (compaction depends on merge order), but its p99 keeps
/// the rank bound: within 2% of the samples of each job's hour from the
/// true p99's rank, at every step.
#[test]
fn cover_kll_quantiles_keep_their_rank_bound() {
    let plan = levels_plan(Built::KllP99, &[WIDE_MS, NARROW_MS]);
    for (t, [cover, flat]) in answers_by_step(&plan) {
        for job in ["a", "b"] {
            let mut values: Vec<f64> = samples()
                .into_iter()
                .filter(|(j, sample_t, _)| *j == job && t - HOUR_MS < *sample_t && *sample_t <= t)
                .map(|(_, _, value)| value)
                .collect();
            values.sort_by(f64::total_cmp);
            let rank = |estimate: f64| {
                values.iter().filter(|&&value| value <= estimate).count() as f64
                    / values.len() as f64
            };
            for (name, answer) in [("cover", &cover), ("flat", &flat)] {
                let error = (rank(answer[job]) - 0.99).abs();
                assert!(error <= 0.02, "{name} {job} at {t}: rank error {error}");
            }
        }
    }
}

/// Nested levels collapse exactly: over DDSketch states, a cover over 5 min,
/// 1 min and 15 s levels (each built from the next narrower) reads the
/// same p99 as the 15 s chain, bit for bit, at every step.
#[test]
fn nested_ddsketch_levels_collapse_exactly() {
    let plan = levels_plan(Built::DDSketchP99, &[WIDE_MS, 60 * SECOND_MS, NARROW_MS]);
    for (t, [cover, flat]) in answers_by_step(&plan) {
        assert_eq!(cover.len(), 2, "at {t}");
        assert_eq!(cover, flat, "at {t}");
    }
}

/// A query run off the narrowest level's grid is refused, not answered
/// from part of the range: at T = 3 h − 7 s the 15 s level's windows
/// cannot tile `(T − 1h, T]`.
#[test]
fn a_cover_off_its_grid_is_refused() {
    let plan = levels_plan(Built::Sum, &[WIDE_MS, NARROW_MS]);
    let mut store = Arc::new(store_with_samples());
    refresh_levels(&plan, &mut store, 3 * HOUR_MS);
    let query = compile(&plan.dag, &plan.views, &[plan.cover as u64]).unwrap();
    let context = RunContext::new(
        Scope::Query {
            evaluation_time_ms: 3 * HOUR_MS - 7 * SECOND_MS,
        },
        Limits::default(),
    )
    .unwrap();
    let query = query.instantiate(store).unwrap();
    let result = crate::common::run_roots(&query, &[plan.cover as u64], context);
    let error = result
        .err()
        .map(|error| error.to_string())
        .unwrap_or_default();
    assert!(error.contains("not on the grid"), "{error:?}");
}

/// Covers of `ranges_ms` over one 5 min level and one 15 s chain (exact
/// sums), and nothing else reading the levels; each step's sums per cover
/// and the sums computed directly over the raw samples, every 15 s over
/// the last 5 minutes of 3 h.
/// One cover's answer at one step: `(T, range in ms, answered, expected)`.
type CoverAnswer = (i64, i64, BTreeMap<String, f64>, BTreeMap<String, f64>);

fn covers_only(ranges_ms: &[i64]) -> Vec<CoverAnswer> {
    let narrow = chain(NARROW_MS, Built::Sum);
    let wide = node(Operator::ASAP(ASAPOp::SummaryMergeWindows {
        child: narrow.clone(),
        tumbling_window_width_ms: WIDE_MS as u64,
    }));
    let covers: Vec<Rc<OperatorNode>> = ranges_ms
        .iter()
        .map(|&range_ms| {
            finalized_merge(
                node(Operator::NonASAP(NonASAPOp::TimeRangeCover {
                    range: Duration::from_millis(range_ms as u64),
                    levels: vec![wide.clone(), narrow.clone()],
                })),
                Built::Sum,
            )
        })
        .collect();
    let roots: Vec<QueryRoot> = covers.iter().cloned().map(QueryRoot::Operator).collect();
    let (mut dag, nodes) =
        planner_types::test_support::forwarders::physical_asap_dag_at_query_time_with_node_ids(
            &roots,
        )
        .unwrap();
    let levels = vec![
        (id_of(&nodes, &narrow), NARROW_MS),
        (id_of(&nodes, &wide), WIDE_MS),
    ];
    for node in dag
        .nodes
        .iter_mut()
        .filter(|node| levels.iter().any(|(id, _)| *id == node.id))
    {
        node.retention_ms = Some(3 * HOUR_MS as u64);
    }
    let ids: Vec<u64> = levels.iter().map(|(id, _)| *id as u64).collect();
    let views = views_at(&mut dag, &ids).unwrap();
    let cover_ids: Vec<u64> = covers
        .iter()
        .map(|cover| id_of(&nodes, cover) as u64)
        .collect();
    let plan = Levels {
        dag,
        levels,
        cover: cover_ids[0] as usize,
        flat: cover_ids[0] as usize,
        views,
    };
    let mut store = Arc::new(store_with_samples());
    refresh_levels(&plan, &mut store, 3 * HOUR_MS);
    let query = compile(&plan.dag, &plan.views, &cover_ids).unwrap();
    let mut answers = Vec::new();
    let mut t = 3 * HOUR_MS - WIDE_MS;
    while t <= 3 * HOUR_MS {
        let context = RunContext::new(
            Scope::Query {
                evaluation_time_ms: t,
            },
            Limits::default(),
        )
        .unwrap();
        let rows = {
            let query = query.instantiate(store.clone()).unwrap();
            run(&query, &cover_ids, context)
        };
        for (cover, &range_ms) in ranges_ms.iter().enumerate() {
            answers.push((
                t,
                range_ms,
                sums_of(rows[cover].clone()),
                direct_sums(t - range_ms, t),
            ));
        }
        t += NARROW_MS;
    }
    answers
}

/// A cover that is the only reader of its levels reads only its own
/// windows from the store and answers the sums over the raw samples.
#[test]
fn a_cover_as_its_levels_only_reader_answers_exactly() {
    for (t, range_ms, answered, expected) in covers_only(&[HOUR_MS]) {
        assert_eq!(answered, expected, "{range_ms} ms at {t}");
    }
}

/// Two covers of different ranges (1 h and 70 min) over the same levels,
/// the levels' only readers: each level is read for the union of the
/// windows both take, so each answers its own range's sums, at every step.
#[test]
fn covers_of_different_ranges_over_shared_levels_answer_exactly() {
    for (t, range_ms, answered, expected) in covers_only(&[HOUR_MS, 70 * 60 * SECOND_MS]) {
        assert_eq!(answered, expected, "{range_ms} ms at {t}");
    }
}
