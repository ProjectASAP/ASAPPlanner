//! An exact aggregate of cheap measures (sums) over many groups stops near
//! its memory limit: the per-group state charge, not the group table or
//! the input, stops it. Its own test binary, so its counting allocator
//! measures this test alone.
use asap_executor::arrow::array::{Float64Array, Int64Array};
use asap_executor::operators::{Operator, Reduction};
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

// `SELECT k, count(DISTINCT x), avg(v) GROUP BY k` over one batch of
/// Run `measures` over `batch`, grouped by `groups`, under `max_bytes`;
/// the result's first batch, and the bytes allocated past the baseline.
fn aggregate(
    input: Arc<Schema>,
    batch: Batch,
    groups: Vec<usize>,
    measures: Vec<(String, Reduction)>,
    max_bytes: usize,
) -> (Result<Batch, Error>, usize) {
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
        Operator::aggregate(input, groups, measures).unwrap(),
    )
    .unwrap();
    let context = RunContext::new(
        Scope::Query {
            evaluation_time_ms: 0,
            revision: 1,
        },
        Limits {
            max_bytes,
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
            .map(|batch| (*batch).clone())
    });
    (result, PEAK.load(Ordering::Relaxed) - baseline)
}

// 200,000 groups with twenty sums each (cheap measures: one f64 per group
// per sum, about 32 MB) under a 16 MiB limit. The input (3.2 MB) and the
// group table fit; only the per-group state charge stops the aggregate.
#[test]
fn many_groups_of_cheap_measures_fail_near_the_memory_limit() {
    const ROWS: i64 = 200_000;
    const LIMIT_BYTES: usize = 16 << 20;
    let field = |name: &str, dtype| Field {
        name: name.into(),
        dtype: FieldDataType::Plain(dtype),
        nullable: false,
        table: None,
    };
    let input = Arc::new(Schema {
        fields: vec![field("k", DataType::Int64), field("v", DataType::Float64)],
        time_index: None,
        unique_keys: vec![],
        closed: false,
    });
    let batch = Batch::try_from_columns(
        input.clone(),
        vec![
            BatchColumn::Plain(Arc::new(Int64Array::from_iter_values(0..ROWS))),
            BatchColumn::Plain(Arc::new(Float64Array::from_iter_values(
                (0..ROWS).map(|i| i as f64),
            ))),
        ],
    )
    .unwrap();
    let measures = (0..20)
        .map(|i| (format!("sum{i}"), Reduction::Sum(1)))
        .collect();
    let (result, allocated) = aggregate(input, batch, vec![0], measures, LIMIT_BYTES);
    assert!(
        matches!(result, Err(Error::MemoryLimit)),
        "{:?}",
        result.map(|b| b.num_rows())
    );
    assert!(
        allocated < 2 * LIMIT_BYTES,
        "allocated {} MiB under a {} MiB limit",
        allocated >> 20,
        LIMIT_BYTES >> 20
    );
}
