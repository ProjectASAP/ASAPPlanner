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
    DDSketchP99,
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
            Built::DDSketchP99 => FieldDataType::Sketch(SketchAlgorithm::DDSketch),
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
        Built::Sum => ASAPOp::FinalizeExactAccumulator {
            counter_readout: None,
            child: merged,
        },
        Built::DDSketchP99 => ASAPOp::SummaryEstimate {
            summary_input: merged,
            query: SketchStatistic::Quantile { q: 0.99 },
        },
    }))
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
        if let Built::DDSketchP99 = built {
            node.parameters = Some(SketchParams::DDSketch { alpha: 0.01 });
        }
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
