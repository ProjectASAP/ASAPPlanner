//! Pass 1 alternatives over SQL row sources compose, compile and execute.
mod executor_models;
mod physical_common;
use std::collections::BTreeMap;
use std::rc::Rc;

use asap_executor::values::Value;
use asap_frontend_sql::{lower_sql, SqlCatalog};
use asap_logical_optimizer::pass1::logical_candidates::{
    compose_logical_candidate, enumerate_choices, enumerate_local_logical_candidates,
};
use asap_plan_selection::plan_stages;
use asap_types::ir::schema::{DataType, Field, Schema};
use asap_types::ir::{ASAPOp, Operator, OperatorNode, QueryRoot};
use asap_types::types::AccuracyTarget;
use asap_types::workload::{
    DataArrival, DataWorkload, Evidence, EvidenceSource, Predictability, QueryRecurrence, Rate,
    RootDemand,
};
use executor_models::executor_models;

fn catalog() -> SqlCatalog {
    SqlCatalog::new().with_table(
        "flows",
        Schema::new(vec![
            Field::plain("ts", DataType::Timestamp, false),
            Field::plain("src_ip", DataType::Utf8, false),
        ]),
    )
}

fn target() -> AccuracyTarget {
    AccuracyTarget::EpsilonDelta {
        epsilon: 0.01,
        delta: 0.01,
    }
}

fn rows() -> Vec<Vec<Value>> {
    ["a", "a", "b", "a", "c", "b"]
        .into_iter()
        .map(|ip| vec![Value::Timestamp(0), Value::Utf8(ip.into())])
        .collect()
}

/// The ε = 0.1 target: at 0.01 a HydraCms grid exceeds the default memory
/// limit, so Stage 3 would not price it.
fn coarse() -> AccuracyTarget {
    AccuracyTarget::EpsilonDelta {
        epsilon: 0.1,
        delta: 0.01,
    }
}

/// SQL `COUNT(*)`, grouped, ungrouped and ranked, each with the rows it
/// returns from [`rows`].
fn count_star_queries() -> [(&'static str, Vec<Vec<Value>>); 3] {
    let count = |ip: &str, n| vec![Value::Utf8(ip.into()), Value::Int64(n)];
    [
        (
            "SELECT src_ip, COUNT(*) AS c FROM flows GROUP BY src_ip",
            vec![count("a", 3), count("b", 2), count("c", 1)],
        ),
        ("SELECT COUNT(*) AS c FROM flows", vec![vec![Value::Int64(6)]]),
        (
            "SELECT src_ip, COUNT(*) AS c FROM flows GROUP BY src_ip ORDER BY COUNT(*) DESC LIMIT 2",
            vec![count("a", 3), count("b", 2)],
        ),
    ]
}

/// SQL `COUNT(*)` (#509 Example 2's inner query) reads no sample value, and
/// a group's rows all hash its key, so Pass 1 offers no per-group sketch.
/// Every candidate composes, compiles, binds and executes to the exact
/// counts: HydraCms too, whose few groups in a wide grid never collide.
#[tokio::test]
async fn sql_count_star_candidates_compose_and_execute() {
    for (sql, expected) in count_star_queries() {
        let expected: Vec<_> = expected.iter().map(|row| format!("{row:?}")).collect();
        let root = lower_sql(sql, &catalog(), coarse()).await.unwrap();
        let inventory = enumerate_local_logical_candidates(
            vec![(0, QueryRoot::Operator(root))],
            &BTreeMap::new(),
        )
        .unwrap();
        let choices = enumerate_choices(&inventory, usize::MAX);
        assert!(choices.len() >= 2, "{sql}: pass-through and exact Count");
        for choice in choices {
            let roots = compose_logical_candidate(&inventory, &choice)
                .unwrap_or_else(|e| panic!("{sql}: {choice:?} composes: {e}"));
            let QueryRoot::Operator(root) = &roots[0].1 else {
                panic!("operator root")
            };
            let mut rows: Vec<_> = physical_common::execute_raw_rows(root, rows())
                .iter()
                .map(|row| format!("{row:?}"))
                .collect();
            rows.sort();
            assert_eq!(rows, expected, "{sql}: {choice:?}");
        }
    }
}

/// Binds every root of `dag` in the executor, each scan reading an empty
/// in-memory source.
fn binds(dag: &asap_types::ir::export::PhysicalASAPDAG) -> Result<(), String> {
    use asap_executor::physical_planner::bind_with_data_sources;
    use asap_executor::sources::{DataSources, MemorySource};
    use asap_types::ir::export::{NonASAPOpKind, PhysicalASAPOperatorPayload};
    use std::sync::Arc;
    let mut sources = DataSources::default();
    let mut registered = vec![];
    for node in &dag.nodes {
        if let PhysicalASAPOperatorPayload::Relational {
            operator: NonASAPOpKind::Scan { source, .. },
        } = &node.payload
        {
            if !registered.contains(source) {
                let schema = Arc::new(node.output_schema.clone());
                sources
                    .register(
                        source.clone(),
                        Arc::new(MemorySource::new(schema, vec![]).unwrap()),
                    )
                    .unwrap();
                registered.push(source.clone());
            }
        }
    }
    let roots: Vec<_> = dag.roots.iter().map(|root| u64::from(root.0)).collect();
    bind_with_data_sources(dag, BTreeMap::new(), &roots, &sources)
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// Every physical candidate Stage 3 prices binds in the executor: a priced
/// plan the executor cannot run could be selected.
async fn assert_priced_candidates_bind(queries: &[&str], target: AccuracyTarget) {
    let mut roots = vec![];
    for (i, sql) in queries.iter().enumerate() {
        let root = lower_sql(sql, &catalog(), target.clone()).await.unwrap();
        roots.push((i, QueryRoot::Operator(root)));
    }
    let demand = vec![
        RootDemand {
            accuracy: Some(target),
            recurrence: QueryRecurrence::OneTime {
                invocations: 1,
                execute_at: None,
            },
            predictability: Predictability::default(),
            latency_ms: None,
        };
        roots.len()
    ];
    let data = DataWorkload {
        arrival: DataArrival::ContinuouslyIngesting,
        ingestion_rate: declared(Rate(100_000.0)),
        input_cardinality: declared(10_000_000),
        ..Default::default()
    };
    let run = plan_stages(roots, &demand, &data, executor_models(), 4096).unwrap();
    let enumeration = run.enumeration.unwrap();
    let mut priced = 0;
    for physical in enumeration.candidates.iter().flat_map(|c| &c.physical) {
        if enumeration.selection.costs.contains_key(&physical.id) {
            binds(&physical.dag).unwrap_or_else(|e| panic!("{queries:?}: {}: {e}", physical.id));
            priced += 1;
        }
    }
    assert!(
        priced > 0,
        "{queries:?}: {:#?}",
        enumeration.selection.rejected
    );
}

#[tokio::test]
async fn priced_sql_count_star_candidates_bind() {
    for (sql, _) in count_star_queries() {
        assert_priced_candidates_bind(&[sql], coarse()).await;
    }
}

fn declared<T>(value: T) -> Evidence<T> {
    Evidence {
        value: Some(value),
        source: EvidenceSource::Declared,
        ..Default::default()
    }
}

/// #509 Example 2's design queries, without their time window: the
/// runtime has no `now()` to bind it with.
const EXAMPLE2: [&str; 3] = [
    "SELECT COUNT(DISTINCT src_ip) FROM flows",
    "SELECT -SUM(p * LN(p)) FROM (SELECT COUNT(*) * 1.0 / SUM(COUNT(*)) OVER () AS p FROM flows GROUP BY src_ip)",
    "SELECT SQRT(SUM(c * c)) FROM (SELECT src_ip, COUNT(*) AS c FROM flows GROUP BY src_ip)",
];

/// Example 2 with Q3's floating product (as `planner_layering_example2`'s
/// `Q3_FLOAT`): the integer Q3's exact `Sum` over Int64 `c * c` is priced
/// but does not bind, since the executor sums only Float64 columns.
#[tokio::test]
async fn priced_example2_candidates_bind() {
    let queries = [
        EXAMPLE2[0],
        EXAMPLE2[1],
        "SELECT SQRT(SUM(CAST(c AS DOUBLE) * CAST(c AS DOUBLE))) FROM (SELECT src_ip, COUNT(*) AS c FROM flows GROUP BY src_ip)",
    ];
    assert_priced_candidates_bind(&queries, target()).await;
}

/// #509 Example 2's design queries (its integer Q3 keeps `COUNT(*) GROUP BY
/// src_ip` as a target): every candidate builds through Stages 1 and 2.
/// Stage 3 may still reject one, e.g. for accuracy.
#[tokio::test]
async fn example2_design_candidates_all_build() {
    let mut roots = vec![];
    for (i, sql) in EXAMPLE2.into_iter().enumerate() {
        let root = lower_sql(sql, &catalog(), target()).await.unwrap();
        roots.push((i, QueryRoot::Operator(root)));
    }
    let demand = vec![
        RootDemand {
            accuracy: Some(target()),
            recurrence: QueryRecurrence::OneTime {
                invocations: 1,
                execute_at: None,
            },
            predictability: Predictability::default(),
            latency_ms: None,
        };
        roots.len()
    ];
    let data = DataWorkload {
        arrival: DataArrival::ContinuouslyIngesting,
        ingestion_rate: declared(Rate(100_000.0)),
        input_cardinality: declared(10_000_000),
        ..Default::default()
    };
    let run = plan_stages(roots, &demand, &data, executor_models(), 4096).unwrap();
    let enumeration = run.enumeration.unwrap();
    let unbuilt: Vec<_> = enumeration
        .selection
        .rejected
        .iter()
        .filter(|r| r.reason.starts_with("Stage "))
        .collect();
    assert!(enumeration.candidates.len() > 1);
    assert!(unbuilt.is_empty(), "{unbuilt:#?}");
}

/// The HydraCms `SummaryAgg`s in `root` (#600's planner contract).
fn hydra_builds(root: &Rc<OperatorNode>) -> Vec<Rc<OperatorNode>> {
    use asap_types::ir::schema::{GroupingStrategy, HydraKind};
    OperatorNode::reachable(root)
        .into_iter()
        .filter(|n| {
            matches!(
                &n.operator,
                Operator::ASAP(ASAPOp::SummaryAgg {
                    grouping: GroupingStrategy::SharedMultiSubpopulation {
                        kind: HydraKind::HydraCms,
                        ..
                    },
                    ..
                })
            )
        })
        .collect()
}

/// A grouped approximate count gets a HydraCms alternative (#580 W7): one
/// shared Count-Min grid for every `src_ip`, built from a unit weight and a
/// non-null item column. Stage 3 prices it (its grouping-aware guarantee
/// meets the target), and it compiles and executes.
#[tokio::test]
async fn grouped_count_offers_a_priced_executable_hydra_plan() {
    use asap_types::ir::schema::{
        FieldDataType, GroupingStrategy, HydraParams, SketchParams, SummaryInputExpr, WeightDomain,
    };
    let sql = "SELECT src_ip, COUNT(*) AS c FROM flows GROUP BY src_ip";
    // The grid holds shared_rows × shared_columns Count-Min cells, each as
    // large as one per-group sketch: ε = 0.01 exceeds the default memory
    // limit, so this uses ε = 0.1.
    let target = AccuracyTarget::EpsilonDelta {
        epsilon: 0.1,
        delta: 0.01,
    };
    let root = lower_sql(sql, &catalog(), target.clone()).await.unwrap();
    let demand = [RootDemand {
        accuracy: Some(target),
        recurrence: QueryRecurrence::OneTime {
            invocations: 1,
            execute_at: None,
        },
        predictability: Predictability::default(),
        latency_ms: None,
    }];
    let data = DataWorkload {
        arrival: DataArrival::ContinuouslyIngesting,
        ingestion_rate: declared(Rate(100_000.0)),
        input_cardinality: declared(10_000_000),
        ..Default::default()
    };
    let run = plan_stages(
        vec![(0, QueryRoot::Operator(root))],
        &demand,
        &data,
        executor_models(),
        4096,
    )
    .unwrap();
    let enumeration = run.enumeration.unwrap();
    let hydra: Vec<_> = enumeration
        .candidates
        .iter()
        .filter_map(|c| {
            let QueryRoot::Operator(root) = &c.logical.as_ref()?[0].1 else {
                return None;
            };
            (!hydra_builds(root).is_empty()).then(|| (c, root.clone()))
        })
        .collect();
    assert!(!hydra.is_empty(), "a Hydra candidate is generated");
    for (candidate, root) in hydra {
        // The all-query-time physical candidate comes first (#604).
        let physical = candidate.physical.first().expect("Stage 2 builds it");
        assert!(
            enumeration.selection.costs.contains_key(&physical.id),
            "{} is priced: {:?}",
            physical.id,
            enumeration.selection.rejected
        );
        for build in hydra_builds(&root) {
            let Operator::ASAP(ASAPOp::SummaryAgg {
                family: FieldDataType::Sketch(kind, family_grouping),
                input,
                grouping,
                filter: None,
                ..
            }) = &build.operator
            else {
                panic!("HydraCms build")
            };
            assert_eq!(family_grouping, grouping);
            let GroupingStrategy::SharedMultiSubpopulation {
                params: HydraParams::HydraCms { width, depth, .. },
                ..
            } = grouping
            else {
                panic!("HydraCms params")
            };
            assert_eq!(
                kind.params(),
                &SketchParams::Cms {
                    width: *width,
                    depth: *depth
                }
            );
            assert!(matches!(input.item, Some(SummaryInputExpr::Column(_))));
            assert_eq!(input.weight, SummaryInputExpr::Constant(1.0));
            assert!(matches!(
                input.weight_domain,
                WeightDomain::NonNegative { .. }
            ));
        }
        let mut rows = physical_common::execute_raw_rows(&root, rows());
        rows.sort_by_key(|row| format!("{row:?}"));
        let counts: Vec<_> = rows
            .iter()
            .map(|row| match (&row[0], &row[1]) {
                (Value::Utf8(ip), Value::Int64(n)) => (ip.to_string(), *n),
                other => panic!("unexpected row {other:?}"),
            })
            .collect();
        // Few groups in a wide grid: no collisions, so the estimate is exact.
        assert_eq!(
            counts,
            [("a".into(), 3), ("b".into(), 2), ("c".into(), 1)],
            "{}",
            physical.id
        );
    }
}
