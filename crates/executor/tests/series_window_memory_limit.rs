//! A subquery over many series stops near its memory limit: SeriesWindow
//! charges its output as it grows, step by step.
use asap_executor::operators::{Operator, SubquerySteps};
use asap_executor::plan::PhysicalDAG;
use asap_executor::planner::ir::operator::AggIntent;
use asap_executor::planner::ir::schema::{DataType, Field, FieldDataType, Schema};
use asap_executor::runtime::{Limits, RunContext, Scope};
use asap_executor::values::{Batch, Value};
use asap_executor::Error;
use futures::{executor::block_on, StreamExt};
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

/// The system allocator, counting the bytes allocated now and at peak.
struct Counting;
static ALLOCATED: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let now = ALLOCATED.fetch_add(layout.size(), Ordering::Relaxed) + layout.size();
        PEAK.fetch_max(now, Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        ALLOCATED.fetch_sub(layout.size(), Ordering::Relaxed);
        unsafe { System.dealloc(ptr, layout) }
    }
}
#[global_allocator]
static GLOBAL: Counting = Counting;

// `max_over_time(x[1h:1s])` over 2,000 series, each with one sample,
// under a 16 MiB limit: 7.2 million output entries would need over 100 MiB.
// The window fails with MemoryLimit having allocated under twice the limit.
#[test]
fn a_long_subquery_over_many_series_fails_near_the_memory_limit() {
    const LIMIT_BYTES: usize = 16 << 20;
    let field = |name: &str, dtype| Field {
        name: name.into(),
        dtype: FieldDataType::Plain(dtype),
        nullable: false,
        table: None,
    };
    let input = Arc::new(Schema {
        fields: vec![
            field("series", DataType::Utf8),
            field("ts", DataType::Timestamp),
            field("value", DataType::Float64),
        ],
        time_index: Some(1),
        unique_keys: vec![],
        closed: false,
    });
    let rows = (0..2_000)
        .map(|i| {
            vec![
                Value::Utf8(format!("s{i}").into()),
                Value::Timestamp(1_000),
                Value::Float64(i as f64),
            ]
        })
        .collect();
    let batch = Batch::try_new(input.clone(), rows).unwrap();
    let window = Operator::series_window(
        input.clone(),
        Some(AggIntent::Max { col: None }),
        3_600_000,
        0,
        None,
        Some(SubquerySteps {
            range_ms: 3_600_000,
            step_ms: 1_000,
            offset_ms: 0,
            at_ms: None,
        }),
    )
    .unwrap();
    let mut dag = PhysicalDAG::default();
    dag.add(0, vec![], Operator::source(input, vec![batch]).unwrap())
        .unwrap();
    dag.add(1, vec![0], window).unwrap();
    let context = RunContext::new(
        Scope::Query {
            evaluation_time_ms: 3_600_000,
            revision: 1,
        },
        Limits {
            max_bytes: LIMIT_BYTES,
            ..Limits::default()
        },
    )
    .unwrap();
    let baseline = ALLOCATED.load(Ordering::Relaxed);
    PEAK.store(baseline, Ordering::Relaxed);
    let result = block_on(async {
        dag.execute(&[1], context)
            .unwrap()
            .remove(0)
            .next()
            .await
            .unwrap()
            .map(|_| ())
    });
    assert!(matches!(result, Err(Error::MemoryLimit)), "{result:?}");
    let overshoot = PEAK.load(Ordering::Relaxed) - baseline;
    assert!(
        overshoot < 2 * LIMIT_BYTES,
        "allocated {} MiB under a {} MiB limit",
        overshoot >> 20,
        LIMIT_BYTES >> 20
    );
}
