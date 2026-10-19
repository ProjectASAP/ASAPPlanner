//! A summary build over one large batch stops near its memory limit, not
//! after allocating every group's state: the build charges new groups as it
//! adds them, a bounded number of rows at a time.
use asap_executor::operators::Operator;
use asap_executor::plan::PhysicalDAG;
use asap_executor::planner::ir::schema::{
    DataType, Field, FieldDataType, Schema, SketchAlgorithm, SketchKind, SketchParams,
};
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

// One batch of 50,000 series, a KLL per series, under a 16 MiB limit: the
// build fails with MemoryLimit having allocated at most a few MiB past the
// limit (one bounded chunk of groups), not one KLL per series first.
#[test]
fn one_large_batch_fails_near_the_memory_limit() {
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
            field("value", DataType::Float64),
        ],
        time_index: None,
        unique_keys: vec![],
        closed: false,
    });
    let rows = (0..50_000)
        .map(|i| vec![Value::Utf8(format!("s{i}").into()), Value::Float64(1.0)])
        .collect();
    let batch = Batch::try_new(input.clone(), rows).unwrap();
    let family = FieldDataType::Sketch(
        SketchKind::new(
            SketchAlgorithm::Kll,
            SketchParams::Kll {
                k: 200,
                failure_probability: None,
            },
        ),
        Default::default(),
    );
    let mut dag = PhysicalDAG::default();
    dag.add(
        0,
        vec![],
        Operator::source(input.clone(), vec![batch]).unwrap(),
    )
    .unwrap();
    dag.add(
        1,
        vec![0],
        Operator::per_group_summary_build(input, family, 1, None, vec![0]).unwrap(),
    )
    .unwrap();
    let context = RunContext::new(
        Scope::Query {
            evaluation_time_ms: 0,
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
