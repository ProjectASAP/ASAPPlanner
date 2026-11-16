//! A grouped Limit over one large batch with many groups stops near its
//! memory limit: it finds groups a bounded number of rows at a time and
//! charges its group table after each chunk.
use asap_executor::arrow::array::{
    ArrayRef, DictionaryArray, Float64Array, Int64Array, StringArray, UInt32Array,
};
use asap_executor::arrow::datatypes::UInt32Type;
use asap_executor::operators::Operator;
use asap_executor::plan::PhysicalDAG;
use asap_executor::planner::ir::schema::{DataType, Field, FieldDataType, Schema};
use asap_executor::runtime::{Limits, RunContext, Scope};
use asap_executor::values::{Batch, BatchColumn};
use asap_executor::Error;
use futures::{executor::block_on, StreamExt};
use planner_types::stages::Lifetime;
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

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

/// One test at a time: the allocation counters are process-wide.
static SERIAL: Mutex<()> = Mutex::new(());

const ROWS: usize = 1_000_000;
const LIMIT_BYTES: usize = 16 << 20;

fn plain_field(name: &str, dtype: DataType) -> Field {
    Field {
        name: name.into(),
        dtype: FieldDataType::Plain(dtype),
        nullable: false,
    }
}

/// Runs `LIMIT 1` per group of all of `fields` over one batch of `columns`
/// under a 16 MiB limit. Returns the run's result and the bytes it
/// allocated at its peak.
fn limit_one_per_group(fields: Vec<Field>, columns: Vec<ArrayRef>) -> (Result<(), Error>, usize) {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let group_columns = (0..fields.len()).collect::<Vec<_>>();
    let input = Arc::new(Schema {
        fields,
        time_index: None,
        unique_keys: vec![],
        closed: false,
    });
    let batch = Batch::try_from_columns(
        input.clone(),
        columns.into_iter().map(BatchColumn::Plain).collect(),
    )
    .unwrap();
    let mut dag = PhysicalDAG::default();
    dag.add(
        0,
        vec![],
        Operator::source(input.clone(), vec![batch]).unwrap(),
        Lifetime::None,
    )
    .unwrap();
    dag.add(
        1,
        vec![0],
        Operator::limit(input, 1, 0, group_columns).unwrap(),
        Lifetime::None,
    )
    .unwrap();
    let context = RunContext::new(
        Scope::Query {
            evaluation_time_ms: 0,
        },
        Limits {
            max_bytes: LIMIT_BYTES,
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
    (result, PEAK.load(Ordering::Relaxed) - baseline)
}

fn assert_fails_near_the_limit(case: &str, (result, overshoot): (Result<(), Error>, usize)) {
    assert!(
        matches!(result, Err(Error::MemoryLimit)),
        "{case}: {result:?}"
    );
    assert!(
        overshoot < 2 * LIMIT_BYTES,
        "{case}: allocated {} MiB under a {} MiB limit",
        overshoot >> 20,
        LIMIT_BYTES >> 20
    );
}

// `LIMIT 1` per group over one batch of 1,000,000 rows, each its own group,
// under a 16 MiB limit: the group table would need far more. The limit
// fails with MemoryLimit having allocated under twice the limit.
#[test]
fn many_groups_in_one_batch_fail_near_the_memory_limit() {
    assert_fails_near_the_limit(
        "Int64",
        limit_one_per_group(
            vec![plain_field("k", DataType::Int64)],
            vec![Arc::new(Int64Array::from_iter_values(0..ROWS as i64))],
        ),
    );
}

// The same over keys of other layouts: a Float64 key, and a (text, Int64)
// key whose text is a dictionary of 1,000,000 entries. The group table
// charges every kind of key it holds.
#[test]
fn many_float_and_text_int_groups_in_one_batch_fail_near_the_memory_limit() {
    assert_fails_near_the_limit(
        "Float64",
        limit_one_per_group(
            vec![plain_field("k", DataType::Float64)],
            vec![Arc::new(Float64Array::from_iter_values(
                (0..ROWS).map(|i| i as f64 + 0.5),
            ))],
        ),
    );
    let texts = StringArray::from_iter_values((0..ROWS).map(|i| format!("series-{i:07}")));
    let keys = UInt32Array::from_iter_values(0..ROWS as u32);
    assert_fails_near_the_limit(
        "(Utf8, Int64)",
        limit_one_per_group(
            vec![
                plain_field("t", DataType::Utf8),
                plain_field("k", DataType::Int64),
            ],
            vec![
                Arc::new(DictionaryArray::<UInt32Type>::try_new(keys, Arc::new(texts)).unwrap()),
                Arc::new(Int64Array::from_iter_values(
                    (0..ROWS as i64).map(|i| i % 7),
                )),
            ],
        ),
    );
}
