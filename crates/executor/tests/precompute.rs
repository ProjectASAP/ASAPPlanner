//! A precompute boundary reads raw samples only from a time series scan.
mod common;
use asap_executor::physical_planner::precompute::{boundary_schema, raw_sample_schema};
use common::compile_physical_asap_dag;
use planner_types::ir::operator::{Source, TimeShift};
use planner_types::ir::schema::{DataType, Field, Schema};
use planner_types::ir::{NonASAPOp, Operator, OperatorNode, TimeRangeKind};
use std::{collections::BTreeMap, rc::Rc, time::Duration};

/// `TimeRange(1m)` over a scan of `source` with columns `ts, value, key`.
fn time_range_over(source: Source) -> Rc<OperatorNode> {
    let scan = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Scan {
        source,
        predicates: vec![],
        schema: Schema::with_time_index(
            vec![
                Field::plain("ts", DataType::Timestamp, false),
                Field::plain("value", DataType::Float64, false),
                Field::plain("key", DataType::Utf8, true),
            ],
            0,
            vec![],
        ),
    }))
    .unwrap();
    OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::TimeRange {
        range: Duration::from_secs(60),
        kind: TimeRangeKind::Range,
        child: scan,
    }))
    .unwrap()
}

fn boundary(
    root: &Rc<OperatorNode>,
) -> Result<asap_executor::values::SchemaRef, asap_executor::Error> {
    let dag = compile_physical_asap_dag(root).unwrap();
    let nodes = dag
        .nodes
        .iter()
        .map(|n| (n.id as u64, n))
        .collect::<BTreeMap<_, _>>();
    boundary_schema(nodes[&(dag.roots[0] as u64)], &nodes)
}

#[test]
fn only_a_time_series_range_reads_raw_samples() {
    let series = boundary(&time_range_over(Source::TimeSeries { metric: "m".into() }));
    assert_eq!(series.unwrap(), raw_sample_schema());
    let table = boundary(&time_range_over(Source::Table {
        table_ref: "t".into(),
    }));
    // A table range is not raw samples: it must be a stored population.
    assert_ne!(table.ok(), Some(raw_sample_schema()));
}

/// A range over stacked time shifts of a time series scan still reads raw
/// samples: the boundary looks through every shift, not only the first.
#[test]
fn a_range_over_stacked_shifts_reads_raw_samples() {
    let range = time_range_over(Source::TimeSeries { metric: "m".into() });
    let NonASAPOp::TimeRange {
        range: window,
        kind,
        child: scan,
    } = range.non_asap().unwrap().clone()
    else {
        unreachable!()
    };
    let shifted = |offset_ms, child| {
        OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::TimeShift {
            shift: TimeShift {
                offset_ms,
                at: None,
            },
            child,
        }))
        .unwrap()
    };
    let stacked = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::TimeRange {
        range: window,
        kind,
        child: shifted(60_000, shifted(300_000, scan)),
    }))
    .unwrap();
    assert_eq!(boundary(&stacked).unwrap(), raw_sample_schema());
}
