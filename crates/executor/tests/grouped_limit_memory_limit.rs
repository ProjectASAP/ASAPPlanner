//! A grouped Limit over one large batch with many groups stops near its
//! memory limit: it finds groups a bounded number of rows at a time and
//! charges its group table after each chunk.
use asap_executor::arrow::array::Int64Array;
use asap_executor::operators::Operator;
use asap_executor::plan::PhysicalDAG;
use asap_executor::planner::ir::schema::{DataType, Field, FieldDataType, Schema};
use asap_executor::runtime::{Limits, RunContext, Scope};
use asap_executor::values::{Batch, BatchColumn};
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

// `LIMIT 1` per group over one batch of 1,000,000 rows, each its own group,
// under a 16 MiB limit: the group table would need far more. The limit
// fails with MemoryLimit having allocated under twice the limit.
#[test]
fn many_groups_in_one_batch_fail_near_the_memory_limit() {
    const ROWS: i64 = 1_000_000;
    const LIMIT_BYTES: usize = 16 << 20;
    let input = Arc::new(Schema {
        fields: vec![Field {
            name: "k".into(),
            dtype: FieldDataType::Plain(DataType::Int64),
            nullable: false,
            table: None,
        }],
        time_index: None,
        unique_keys: vec![],
        closed: false,
    });
    let batch = Batch::try_from_columns(
        input.clone(),
        vec![BatchColumn::Plain(Arc::new(Int64Array::from_iter_values(
            0..ROWS,
        )))],
    )
    .unwrap();
    let mut dag = PhysicalDAG::default();
    dag.add(
        0,
        vec![],
        Operator::source(input.clone(), vec![batch]).unwrap(),
    )
    .unwrap();
    dag.add(1, vec![0], Operator::limit(input, 1, 0, vec![0]).unwrap())
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
