//! One scan of a source shared by readers with different predicates,
//! labels, time ranges and shifts (#951).
use asap_devtools::planner_fuzz::{read_seed_file, FuzzSettings, FuzzWorkload, Planner};
use asap_executor::values::Value;
use asap_logical_asap_optimization::{
    memo::{
        candidates::{try_populate_pass1, CandidateMemo, CandidateOrigin, ImplementationCandidate},
        state_configurations::PaneWidthDomain,
        state_groups::expand_concrete_summary_state_groups_in_domain,
        LogicalMemo,
    },
    per_computation_candidates::generate_per_computation_candidates,
};
use asap_types::ir::schema::DataType;
use asap_types::ir::{NonASAPOp, OperatorNode};
use serde_json::json;
use std::path::Path;
use std::rc::Rc;

mod physical_common;

const W5: &str = "ts > now() - INTERVAL '5' MINUTE";

fn fixture(issue: u32) -> FuzzWorkload {
    read_seed_file(
        &Path::new(env!("CARGO_MANIFEST_DIR"))
            .join(format!("tests/sharing_issue_fixtures/{issue}.json")),
    )
    .unwrap()
}

/// `queries` in the #923 PromQL workload (`m_gauge`, 1000 series).
fn promql(queries: &[&str]) -> FuzzWorkload {
    let mut w = fixture(923);
    let template = w.queries[0].clone();
    w.queries = queries
        .iter()
        .map(|query| {
            let mut entry = template.clone();
            entry.entry.query.0 = (*query).into();
            entry
        })
        .collect();
    w
}

/// `queries` in the #902 SQL workload (`metrics`, every 10 s).
fn sql(queries: &[String]) -> FuzzWorkload {
    let mut w = fixture(902);
    w.tables = vec![json!({"name":"metrics", "time_index":0, "columns":[
        {"name":"ts","type":"timestamp","nullable":false},
        {"name":"city","type":"utf8","nullable":true},
        {"name":"host","type":"utf8","nullable":true},
        {"name":"value","type":"float64","nullable":true}
    ]})];
    let template = w.queries[0].clone();
    w.queries = queries
        .iter()
        .map(|query| {
            let mut entry = template.clone();
            entry.entry.query.0 = query.clone();
            entry
        })
        .collect();
    w
}

fn expanded(w: &FuzzWorkload) -> CandidateMemo {
    let roots: Vec<_> = Planner::new(FuzzSettings::default())
        .lower(w)
        .unwrap()
        .into_iter()
        .enumerate()
        .collect();
    let initial = LogicalMemo::from_roots(&roots);
    let inventory = generate_per_computation_candidates(roots).unwrap();
    let memo = try_populate_pass1(initial, &inventory).unwrap();
    expand_concrete_summary_state_groups_in_domain(memo, PaneWidthDomain::SeedGrids)
        .unwrap()
        .memo
}

/// The windowed read (`TimeRange` over a scan) of query `query` alone, its
/// readers of a shared read (with that shared read's group), and the
/// aggregate group directly above it.
fn window_readers(
    memo: &CandidateMemo,
    query: usize,
) -> (usize, Vec<(&ImplementationCandidate, usize)>, usize) {
    let own = |kind: fn(&NonASAPOp) -> bool| {
        memo.groups
            .iter()
            .position(|group| group.queries == [query] && group.node.non_asap().is_some_and(kind))
    };
    let window = own(|op| matches!(op, NonASAPOp::TimeRange { .. })).expect("own window");
    let aggregate = memo
        .groups
        .iter()
        .position(|group| {
            group.queries == [query]
                && matches!(group.node.non_asap(), Some(NonASAPOp::Aggregate { .. }))
                && group
                    .children
                    .contains(&asap_logical_asap_optimization::memo::MemoGroupId(
                        window as u32,
                    ))
        })
        .expect("an aggregate over the window");
    let readers = memo.candidates[window]
        .iter()
        .filter(|c| c.origin == CandidateOrigin::WidenedScans && c.children.len() == 1)
        .map(|c| (c, c.children[0].0 as usize))
        .collect();
    (window, readers, aggregate)
}

/// `node` with its descendant `from` replaced by `to`.
fn replaced(
    node: &Rc<OperatorNode>,
    from: &OperatorNode,
    to: &Rc<OperatorNode>,
) -> Rc<OperatorNode> {
    if std::ptr::eq(&**node, from) {
        return Rc::clone(to);
    }
    Rc::new(
        node.with_new_children(|child| replaced(child, from, to))
            .unwrap(),
    )
}

/// The `Scan` at the bottom of the window `node`.
fn scan_below(node: &Rc<OperatorNode>) -> Rc<OperatorNode> {
    match node.non_asap() {
        Some(NonASAPOp::TimeRange { child, .. } | NonASAPOp::TimeShift { child, .. }) => {
            scan_below(child)
        }
        _ => Rc::clone(node),
    }
}

/// For queries 0 and 1: both have a reader of one shared scan, and each
/// query's aggregate over its reader gives its aggregate's rows on `rows`.
fn assert_readers_of_one_shared_read(memo: &CandidateMemo, rows: &[NamedRow]) {
    let mut shared = Vec::new();
    for query in 0..2 {
        let (window, readers, aggregate) = window_readers(memo, query);
        let (reader, group) = readers.first().expect("a reader of the shared scan");
        shared.push(scan_below(&memo.groups[*group].node));
        let original = &memo.groups[aggregate].node;
        let through_reader = replaced(original, &memo.groups[window].node, &reader.node);
        assert_same_rows(original, &through_reader, rows);
        assert_same_rows(original, &through_reader, &[]);
    }
    assert!(
        Rc::ptr_eq(&shared[0], &shared[1]),
        "both read one shared scan"
    );
}

/// Source rows as named values; each scan reads the columns it has.
type NamedRow = Vec<(&'static str, Value)>;

fn rows_for(node: &OperatorNode, rows: &[NamedRow]) -> Vec<Vec<Value>> {
    let scan = OperatorNode::reachable(&std::rc::Rc::new(node.clone()))
        .into_iter()
        .find(|n| matches!(n.non_asap(), Some(NonASAPOp::Scan { .. })))
        .unwrap();
    rows.iter()
        .map(|row| {
            scan.schema
                .fields
                .iter()
                .map(|field| {
                    if field.name == asap_types::ir::schema::PROMQL_SERIES_IDENTITY {
                        // The identity is every label of the row's series.
                        let labels: Vec<String> = row
                            .iter()
                            .filter(|(name, _)| !["ts", "value"].contains(name))
                            .map(|(name, value)| format!("{name}={value:?}"))
                            .collect();
                        return match field.plain_dtype() {
                            Some(DataType::Utf8) => Value::Utf8(labels.join(",").into()),
                            other => panic!("series identity type {other:?}"),
                        };
                    }
                    row.iter()
                        .find(|(name, _)| *name == field.name)
                        .map_or(Value::Null, |(_, value)| value.clone())
                })
                .collect()
        })
        .collect()
}

/// `reader` gives the rows its original scan `original` gives.
fn assert_same_rows(original: &OperatorNode, reader: &OperatorNode, rows: &[NamedRow]) {
    let sorted = |rows: Vec<Vec<Value>>| {
        let mut rows: Vec<_> = rows.iter().map(|row| format!("{row:?}")).collect();
        rows.sort();
        rows
    };
    let run = |node: &OperatorNode| {
        physical_common::execute_raw_rows_at(
            &std::rc::Rc::new(node.clone()),
            rows_for(node, rows),
            1_000,
        )
    };
    assert_eq!(sorted(run(reader)), sorted(run(original)));
}

/// Example 2: two SQL reads of `metrics` with different cities share one
/// read of either city; each keeps its own city's rows.
#[test]
fn sql_reads_with_different_predicates_share_one_scan() {
    let memo = expanded(&sql(&[
        format!("SELECT SUM(value) FROM metrics WHERE city = 'NYC' AND {W5}"),
        format!("SELECT MAX(value) FROM metrics WHERE city = 'SF' AND {W5}"),
    ]));
    let row = |city: Option<&str>, value: Option<f64>| -> NamedRow {
        vec![
            ("ts", Value::Timestamp(0)),
            ("city", city.map_or(Value::Null, |c| Value::Utf8(c.into()))),
            ("host", Value::Utf8("h1".into())),
            ("value", value.map_or(Value::Null, Value::Float64)),
        ]
    };
    assert_readers_of_one_shared_read(
        &memo,
        &[
            row(Some("NYC"), Some(1.0)),
            row(Some("SF"), Some(2.0)),
            row(Some("LA"), Some(30.0)),
            row(None, Some(4.0)),
            row(Some("NYC"), None),
        ],
    );
}

/// Two SQL reads of `metrics` with different cities and different windows
/// (5 and 1 minutes) share one scan of either city; each reader keeps its
/// own window over it and its own city above the window.
#[test]
fn sql_reads_with_different_predicates_and_windows_share_one_scan() {
    let memo = expanded(&sql(&[
        format!("SELECT SUM(value) FROM metrics WHERE city = 'NYC' AND {W5}"),
        "SELECT MAX(value) FROM metrics WHERE city = 'SF' AND ts > now() - INTERVAL '1' MINUTE"
            .to_string(),
    ]));
    // Rows at 30 s and 4 min before the evaluation time (1000 s): the 1-minute
    // read keeps only the first.
    let row = |ago_s: i64, city: Option<&str>, value: Option<f64>| -> NamedRow {
        vec![
            ("ts", Value::Timestamp((1_000 - ago_s) * 1_000)),
            ("city", city.map_or(Value::Null, |c| Value::Utf8(c.into()))),
            ("host", Value::Utf8("h1".into())),
            ("value", value.map_or(Value::Null, Value::Float64)),
        ]
    };
    assert_readers_of_one_shared_read(
        &memo,
        &[
            row(30, Some("NYC"), Some(1.0)),
            row(240, Some("NYC"), Some(10.0)),
            row(30, Some("SF"), Some(2.0)),
            row(240, Some("SF"), Some(20.0)),
            row(30, Some("LA"), Some(30.0)),
            row(30, None, Some(4.0)),
            row(30, Some("SF"), None),
        ],
    );
}

/// Two PromQL reads of `m_counter` with different matchers, windows and
/// shifts (`{region="us"}[5m]` and `{job="a"}[1m] offset 10m`) share one
/// scan. Run together in one DAG, each reader gives its own read's rows,
/// and the scan's provider is asked once for the union of the windows,
/// `(T - 11m, T]`.
#[test]
fn promql_reads_with_different_windows_and_shifts_share_one_scan() {
    let memo = expanded(&promql(&[
        "rate(m_counter{region=\"us\"}[5m])",
        "rate(m_counter{job=\"a\"}[1m] offset 10m)",
    ]));
    let mut readers = Vec::new();
    let mut originals = Vec::new();
    for query in 0..2 {
        let (window, own_readers, _) = window_readers(&memo, query);
        let (reader, _) = own_readers.first().expect("a reader of the shared scan");
        readers.push(Rc::clone(&reader.node));
        originals.push(Rc::clone(&memo.groups[window].node));
    }
    assert!(Rc::ptr_eq(
        &scan_below_reader(&readers[0]),
        &scan_below_reader(&readers[1])
    ));
    let t_ms = 3_600_000;
    let row = |ago_s: i64, job: &str, region: &str, value: f64| -> NamedRow {
        vec![
            ("ts", Value::Timestamp(t_ms - ago_s * 1_000)),
            ("value", Value::Float64(value)),
            ("instance", Value::Utf8("i1".into())),
            ("job", Value::Utf8(job.into())),
            ("region", Value::Utf8(region.into())),
        ]
    };
    let rows: Vec<NamedRow> = [30, 200, 400, 620, 650, 700]
        .into_iter()
        .flat_map(|ago_s| {
            [("a", "us"), ("a", "eu"), ("b", "us"), ("b", "eu")]
                .map(|(job, region)| row(ago_s, job, region, ago_s as f64))
        })
        .collect();
    let (shared, requests) = run_together(&readers, &rows, t_ms);
    assert_eq!(
        requests,
        [Some((t_ms - 660_000, t_ms))],
        "one request, the union window"
    );
    for (query, original) in originals.iter().enumerate() {
        let (alone, _) = run_together(std::slice::from_ref(original), &rows, t_ms);
        assert!(!alone[0].is_empty(), "query {query} reads rows");
        assert_eq!(
            sorted(shared[query].clone()),
            sorted(alone[0].clone()),
            "query {query}"
        );
    }
}

/// The scan below a reader's `Filter`s and `Project`s and its window.
fn scan_below_reader(node: &Rc<OperatorNode>) -> Rc<OperatorNode> {
    match node.non_asap() {
        Some(NonASAPOp::Filter { child, .. } | NonASAPOp::Project { child, .. }) => {
            scan_below_reader(child)
        }
        _ => scan_below(node),
    }
}

fn sorted(rows: Vec<Vec<Value>>) -> Vec<String> {
    let mut rows: Vec<_> = rows.iter().map(|row| format!("{row:?}")).collect();
    rows.sort();
    rows
}

/// `roots`, one DAG at query time over their one scan, run at `t_ms` with
/// `rows` bound to the scan's source: each root's rows, and each time range
/// the provider was asked for.
fn run_together(
    roots: &[Rc<OperatorNode>],
    rows: &[NamedRow],
    t_ms: i64,
) -> (Vec<Vec<Vec<Value>>>, Vec<RequestedRange>) {
    use asap_executor::physical_planner::compile;
    use asap_executor::runtime::{Limits, RunContext, Scope};
    use asap_executor::sources::{BindingDataSources, DataSources, MemorySource};
    use asap_executor::values::Batch;
    use asap_types::ir::physical_export::{
        compile_physical_asap_workload, PhysicalASAPOperatorPayload,
    };
    use asap_types::ir::{
        apply_materialization_timings_to_roots, MaterializationAssignment, QueryRoot,
    };
    use futures::{executor::block_on, StreamExt};
    use std::sync::{Arc, Mutex};
    let timed = apply_materialization_timings_to_roots(
        &roots
            .iter()
            .map(|root| QueryRoot::Operator(Rc::clone(root)))
            .collect::<Vec<_>>(),
        &MaterializationAssignment::default(),
    )
    .unwrap();
    let dag = compile_physical_asap_workload(&timed).unwrap();
    let (source, schema) = dag
        .nodes
        .iter()
        .find_map(|node| match &node.payload {
            PhysicalASAPOperatorPayload::NonASAP(NonASAPOp::Scan { source, .. }) => {
                Some((source.clone(), node.output_schema.clone()))
            }
            _ => None,
        })
        .expect("one scan");
    let scan = OperatorNode::reachable(&roots[0])
        .into_iter()
        .find(|node| matches!(node.non_asap(), Some(NonASAPOp::Scan { .. })))
        .unwrap();
    let input = Arc::new(schema);
    let batch = Batch::try_new(input.clone(), rows_for(&scan, rows)).unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let mut tables = DataSources::default();
    tables
        .register(
            source,
            Arc::new(Recording {
                rows: MemorySource::new(input, vec![batch]).unwrap(),
                requests: Arc::clone(&requests),
            }),
        )
        .unwrap();
    let ids: Vec<u64> = dag.roots.iter().map(|&id| id as u64).collect();
    let data_sources = BindingDataSources {
        tables,
        ..Default::default()
    };
    let plan = compile(&dag, &Default::default(), &ids)
        .unwrap()
        .instantiate(&data_sources)
        .unwrap();
    let context = RunContext::new(
        Scope::Query {
            evaluation_time_ms: t_ms,
            revision: 1,
        },
        Limits::default(),
    )
    .unwrap();
    let outputs = block_on(async {
        let streams = plan.execute(&ids, context.clone()).unwrap();
        futures::future::join_all(streams.into_iter().map(|mut stream| async move {
            let mut rows = Vec::new();
            while let Some(batch) = stream.next().await {
                rows.extend(batch.unwrap().to_rows());
            }
            rows
        }))
        .await
    });
    let requests = requests.lock().unwrap().clone();
    (outputs, requests)
}

/// A requested `(start, end]` in milliseconds, `None` for every row.
type RequestedRange = Option<(i64, i64)>;

/// A source that records each requested time range and returns every row.
struct Recording {
    rows: asap_executor::sources::MemorySource,
    requests: std::sync::Arc<std::sync::Mutex<Vec<RequestedRange>>>,
}
impl asap_executor::sources::TableProvider for Recording {
    fn schema(&self) -> asap_executor::values::SchemaRef {
        self.rows.schema()
    }
    fn boundedness(&self) -> asap_executor::plan::Boundedness {
        self.rows.boundedness()
    }
    fn scan(
        &self,
        request: &asap_executor::sources::ScanRequest,
        context: asap_executor::runtime::RunContext,
    ) -> Result<
        asap_executor::runtime::OutputStream<'_, asap_executor::values::Batch>,
        asap_executor::Error,
    > {
        self.requests.lock().unwrap().push(request.time_range_ms);
        self.rows.scan(request, context)
    }
}

/// Example 1: a PromQL read with a label matcher and one without share one
/// read of every label; the matcher becomes the reader's filter, and the
/// other reader keeps only its own labels.
#[test]
fn promql_reads_with_and_without_a_matcher_share_one_scan() {
    let memo = expanded(&promql(&[
        "sum by (job) (rate(m_counter[5m]))",
        "sum by (job) (rate(m_counter{region=\"us\"}[5m]))",
    ]));
    // A logical PromQL range read has no native execution before it is
    // lowered, so the readers are checked by shape: the matcher reader keeps
    // `region = "us"`, the other projects away `region`.
    let mut shared = Vec::new();
    for query in 0..2 {
        let (window, readers, _) = window_readers(&memo, query);
        let (reader, group) = readers.first().expect("a reader of the shared read");
        shared.push(*group);
        assert_eq!(
            reader.node.schema.fields,
            memo.groups[window].node.schema.fields
        );
        let kinds: Vec<_> = OperatorNode::reachable(&reader.node)
            .iter()
            .map(|node| node.operator.kind_name())
            .collect();
        assert_eq!(
            kinds.contains(&"Filter"),
            query == 1,
            "only the matcher reader filters: {kinds:?}"
        );
    }
    assert_eq!(shared[0], shared[1], "both read one shared read");
    let Some(NonASAPOp::TimeRange { child, .. }) = memo.groups[shared[0]].node.non_asap() else {
        panic!("the shared read is a window")
    };
    assert!(
        matches!(child.non_asap(), Some(NonASAPOp::Scan { predicates, .. }) if predicates.is_empty())
    );
}

/// Examples 3 and 5: scans of one source that differ only in time range or
/// shift are already one scan group; the windows sit above it.
#[test]
fn scans_differing_only_in_window_or_shift_are_one_group() {
    let w1 = "ts > now() - INTERVAL '1' MINUTE";
    let p90 = "percentile_cont(0.9) WITHIN GROUP (ORDER BY value)";
    for w in [
        sql(&[
            format!("SELECT {p90} FROM metrics WHERE {W5}"),
            format!("SELECT {p90} FROM metrics WHERE {w1}"),
        ]),
        promql(&["rate(m_counter[5m])", "rate(m_counter[5m] offset 2m)"]),
        promql(&["rate(m_counter[5m])", "rate(m_counter[5m] @ 1000)"]),
    ] {
        let memo = expanded(&w);
        assert!(memo.groups.iter().any(|group| {
            matches!(group.node.non_asap(), Some(NonASAPOp::Scan { .. })) && group.queries == [0, 1]
        }));
    }
}

/// The selected plan's cost per second.
fn cost(w: &FuzzWorkload) -> f64 {
    search(w).0
}

/// The selected plan's cost per second and its query roots.
fn search(w: &FuzzWorkload) -> (f64, Vec<asap_types::ir::QueryRoot>) {
    use asap_devtools::planning_stages::run_search_over;
    use asap_plan_selection::{PlanSelectionSettings, PlanningModels, Stage1ToStage3Search};
    use asap_planner::PlannerExecutorCompileCheck;
    let roots = Planner::new(FuzzSettings::default()).lower(w).unwrap();
    let planning = w.planning_workload();
    let entries: Vec<_> = planning.query_workload.entries().collect();
    let settings = PlanSelectionSettings::default();
    let deployment = w.deployment();
    let models = PlanningModels::builtin()
        .with_plan_selection(&settings)
        .with_deployment_capabilities(&deployment)
        .with_executor_compile_check(&PlannerExecutorCompileCheck);
    let run = run_search_over(
        roots,
        &entries,
        &w.data,
        models,
        if std::env::var("DUMP_ALL").is_ok() {
            Stage1ToStage3Search::exhaustive()
        } else {
            Stage1ToStage3Search {
                milp_time_limit_ms: Some(30_000),
                ..Default::default()
            }
        },
    )
    .unwrap();
    let selection = run.stage3_plan_selection.as_ref().unwrap();
    if std::env::var("DUMP_ALL").is_ok() {
        let mut costs: Vec<_> = selection
            .costs
            .iter()
            .filter_map(|(name, cost)| Some((cost.cost_per_second?, name)))
            .collect();
        costs.sort_by(|a, b| a.0.total_cmp(&b.0));
        println!(
            "ALL {} valid, cheapest {:?}",
            costs.len(),
            &costs[..costs.len().min(5)]
        );
        let mut reasons = std::collections::BTreeMap::new();
        for rejection in &selection.rejected {
            *reasons
                .entry(
                    format!("{:?}", rejection)
                        .chars()
                        .take(160)
                        .collect::<String>(),
                )
                .or_insert(0) += 1;
        }
        for (reason, count) in reasons.iter().take(15) {
            println!("REJECT {count} {reason}");
        }
    }
    let value = selection.costs[&selection.selected]
        .cost_per_second
        .unwrap();
    println!("cost={value}; selected={}", selection.selected);
    if std::env::var("DUMP_PLAN").is_ok() {
        // Every distinct node of the selected plan, with its children's ids.
        let mut seen = std::collections::HashSet::new();
        for root in &selection.plan {
            let nodes = match root {
                asap_types::ir::QueryRoot::Operator(node) => vec![node.clone()],
                asap_types::ir::QueryRoot::Scalar(expr) => {
                    expr.operator_refs().into_iter().cloned().collect()
                }
            };
            for node in nodes {
                for n in OperatorNode::reachable(&node) {
                    if !seen.insert(std::rc::Rc::as_ptr(&n)) {
                        continue;
                    }
                    let id = |n: &OperatorNode| format!("{:x}", n as *const _ as usize & 0xffffff);
                    println!(
                        "PLAN {} {} <- {:?} {:?} {}",
                        id(&n),
                        n.operator.kind_name(),
                        n.children().iter().map(|c| id(c)).collect::<Vec<_>>(),
                        n.timing,
                        match n.non_asap() {
                            Some(NonASAPOp::Scan { predicates, .. }) => format!("{predicates:?}"),
                            Some(NonASAPOp::Filter { pred, .. }) => format!("{pred:?}"),
                            Some(NonASAPOp::TimeRange { range, .. }) => format!("{range:?}"),
                            Some(NonASAPOp::TimeShift { shift, .. }) => format!("{shift:?}"),
                            _ => String::new(),
                        }
                    );
                }
            }
        }
        for (id, node) in &selection.costs[&selection.selected].per_node {
            println!("NODECOST {id:?} {node:?}");
        }
    }
    (value, selection.plan.clone())
}

/// End to end: the shared-scan candidates added for two per-city p90
/// queries with different windows do not make the joint plan dearer than
/// planning each query alone (an earlier version of the rule raised the
/// selected cost by 3.8%).
///
/// Measured against the main this branch is based on: each query alone is
/// an ingestion-time KLL pane chain over its own scan, and the joint plan
/// is those two plans, with or without this rule (see the PR's Status).
#[test]
fn shared_scan_candidates_do_not_raise_the_joint_cost() {
    let p90 = "percentile_cont(0.9) WITHIN GROUP (ORDER BY value)";
    let queries = [
        format!("SELECT {p90} FROM metrics WHERE city = 'NYC' AND {W5}"),
        format!("SELECT {p90} FROM metrics WHERE city = 'SF' AND ts > now() - INTERVAL '1' MINUTE"),
    ];
    let together = cost(&sql(&queries));
    let alone: f64 = queries
        .iter()
        .map(|q| cost(&sql(std::slice::from_ref(q))))
        .sum();
    assert!(
        together <= alone * (1.0 + 1e-9),
        "together {together}, alone {alone}"
    );
}

/// `w` with every query evaluated once an hour, where reading raw rows at
/// query time can beat maintaining panes.
fn hourly(mut w: FuzzWorkload) -> FuzzWorkload {
    for query in &mut w.queries {
        query.entry.demand = asap_types::workload::RepeatedDemand::FixedInterval(
            asap_types::workload::RepetitionInterval(3_600_000),
        );
    }
    w
}

/// Prints the selected plans and their node costs for the examples
/// (`DUMP_PLAN=1`): which scans each plan reads and what each costs.
/// `DUMP_ONLY=<name>` runs one example; `DUMP_ALL=1` searches exhaustively.
#[test]
#[ignore = "diagnostic dump"]
fn dump_selected_plans() {
    let w1 = "ts > now() - INTERVAL '1' MINUTE";
    let p90 = "percentile_cont(0.9) WITHIN GROUP (ORDER BY value)";
    for (name, w) in [
        (
            "example 2 at query time",
            sql(&[
                format!("SELECT percentile_cont(0.9) WITHIN GROUP (ORDER BY value) FROM metrics WHERE city = 'NYC' AND {W5}"),
                format!("SELECT percentile_cont(0.9) WITHIN GROUP (ORDER BY value) FROM metrics WHERE city = 'SF' AND {W5}"),
            ]),
        ),
        (
            "example 2",
            sql(&[
                format!("SELECT SUM(value) FROM metrics WHERE city = 'NYC' AND {W5}"),
                format!("SELECT MAX(value) FROM metrics WHERE city = 'SF' AND {W5}"),
            ]),
        ),
        (
            "example 3",
            sql(&[
                format!("SELECT {p90} FROM metrics WHERE {W5}"),
                format!("SELECT {p90} FROM metrics WHERE {w1}"),
            ]),
        ),
        (
            "example 5",
            promql(&["rate(m_counter[5m])", "rate(m_counter[5m] offset 2m)"]),
        ),
        (
            "example 2, different windows",
            sql(&[
                format!("SELECT SUM(value) FROM metrics WHERE city = 'NYC' AND {W5}"),
                format!("SELECT MAX(value) FROM metrics WHERE city = 'SF' AND {w1}"),
            ]),
        ),
        (
            "example 2 at query time, different windows",
            sql(&[
                format!("SELECT {p90} FROM metrics WHERE city = 'NYC' AND {W5}"),
                format!("SELECT {p90} FROM metrics WHERE city = 'SF' AND {w1}"),
            ]),
        ),
        (
            "example 2 at query time, different windows, hourly",
            hourly(sql(&[
                format!("SELECT {p90} FROM metrics WHERE city = 'NYC' AND {W5}"),
                format!("SELECT {p90} FROM metrics WHERE city = 'SF' AND {w1}"),
            ])),
        ),
        (
            "example 2, different windows, hourly",
            hourly(sql(&[
                format!("SELECT SUM(value) FROM metrics WHERE city = 'NYC' AND {W5}"),
                format!("SELECT MAX(value) FROM metrics WHERE city = 'SF' AND {w1}"),
            ])),
        ),
        (
            "example 1, different windows and shifts, hourly",
            hourly(promql(&[
                "sum by (job) (rate(m_counter{region=\"us\"}[5m]))",
                "sum by (job) (rate(m_counter{job=\"a\"}[1m] offset 10m))",
            ])),
        ),
        (
            "example 1",
            promql(&[
                "sum by (job) (rate(m_counter[5m]))",
                "sum by (job) (rate(m_counter{region=\"us\"}[5m]))",
            ]),
        ),
        (
            "example 1, different windows and shifts",
            promql(&[
                "sum by (job) (rate(m_counter{region=\"us\"}[5m]))",
                "sum by (job) (rate(m_counter{job=\"a\"}[1m] offset 10m))",
            ]),
        ),
    ] {
        if std::env::var("DUMP_ONLY").is_ok_and(|only| name != only) {
            continue;
        }
        println!("######## {name}");
        cost(&w);
    }
}
