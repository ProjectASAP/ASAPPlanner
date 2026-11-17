//! A `SummaryMerge` of a chain's stored windows allocates per window and
//! per output group, never per merged state, and a sketch merged in place
//! into a state that already covers its range allocates nothing.
//!
//! Counts the allocations of the test's own thread, so tests running in
//! parallel do not count each other's.
use asap_executor::planner::ir::schema::FieldDataType;
use asap_executor::planner::summary_type::{ExactKind, ExactParams, SketchAlgorithm, SketchParams};
use asap_executor::values::{Batch, Value};
use asap_executor::AggregateCore;
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::Arc;

#[allow(dead_code)]
#[path = "common/stored_windows.rs"]
mod stored_windows;
use stored_windows::{merge, stored_windows, Family, WindowShape};

thread_local! {
    /// Allocations made by this thread so far.
    static ALLOCATIONS: Cell<usize> = const { Cell::new(0) };
}

/// The system allocator, counting each thread's allocations.
struct Counting;
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let _ = ALLOCATIONS.try_with(|count| count.set(count.get() + 1));
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let _ = ALLOCATIONS.try_with(|count| count.set(count.get() + 1));
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}
#[global_allocator]
static GLOBAL: Counting = Counting;

/// The allocations (and reallocations) `work` makes on this thread.
fn allocations_of<T>(work: impl FnOnce() -> T) -> (usize, T) {
    let before = ALLOCATIONS.with(Cell::get);
    let output = work();
    (ALLOCATIONS.with(Cell::get) - before, output)
}

fn families() -> Vec<Family> {
    vec![
        (
            FieldDataType::ExactAggregate(ExactKind::Sum, ExactParams::Sum),
            None,
        ),
        (
            FieldDataType::ExactAggregate(ExactKind::Rate, ExactParams::Rate),
            None,
        ),
        (
            FieldDataType::Sketch(SketchAlgorithm::DDSketch),
            Some(SketchParams::DDSketch { alpha: 0.01 }),
        ),
        (
            FieldDataType::Sketch(SketchAlgorithm::Hll),
            Some(SketchParams::Hll { precision: 10 }),
        ),
    ]
}

/// The allocations of a `SummaryMerge` of the first `windows` windows of
/// `series` series.
fn merge_allocations(family: &Family, series: usize, windows: usize) -> usize {
    let stored = stored_windows(
        family,
        WindowShape {
            series,
            windows,
            samples_per_window: 20,
        },
    );
    merge(&stored); // Warm up lazily allocated statics.
    allocations_of(|| merge(&stored)).0
}

// The allocations that grow with series × windows, isolated by a second
// difference over 100 and 400 series and 12 and 48 windows: a cost per
// window (a batch, a group table) or per output group (its first state's
// copy, its label) cancels out; one allocation per merged state leaves
// 300 × 36 = 10,800. At most 0.01 per merged state remain (amortized
// vector growth). Today a sketch merge copies the running state and boxes
// a new one for every merged state.
#[test]
fn merging_allocates_nothing_per_merged_state() {
    let mut failures = Vec::new();
    for family in families() {
        let [small_few, small_many, large_few, large_many] =
            [(100, 12), (100, 48), (400, 12), (400, 48)]
                .map(|(series, windows)| merge_allocations(&family, series, windows));
        let per_state = (large_many + small_few) as f64 - (large_few + small_many) as f64;
        let per_state = per_state / (300.0 * 36.0);
        println!(
            "{:?}: {small_few} / {small_many} / {large_few} / {large_many} allocations \
             (100 or 400 series x 12 or 48 windows): {per_state:.3} per merged state",
            family.0
        );
        if per_state > 0.01 {
            failures.push(format!("{:?}: {per_state:.3}", family.0));
        }
    }
    assert!(
        failures.is_empty(),
        "allocations per merged state: {failures:?}"
    );
}

/// Series 0's state in each of `windows`.
fn first_states(windows: &[Batch]) -> Vec<Arc<dyn AggregateCore>> {
    windows
        .iter()
        .map(|window| {
            let row = window.to_rows().swap_remove(0);
            let Value::Summary { state, .. } = &row[1] else {
                panic!("a state")
            };
            state.clone()
        })
        .collect()
}

// A state that already covers the range of the states merged into it (a
// DDSketch whose bucket stores span their buckets, an HLL's registers, an
// exact record) merges them in place with no allocation.
#[test]
fn merging_in_place_into_a_covering_state_allocates_nothing() {
    for family in families() {
        let windows = stored_windows(
            &family,
            WindowShape {
                series: 1,
                windows: 8,
                samples_per_window: 200,
            },
        );
        let states = first_states(&windows);
        // The running state first covers every input's range.
        let mut running = states[0].clone_boxed_core();
        for state in &states[1..] {
            running.merge_in_place(state.as_ref()).unwrap();
        }
        let (allocations, _) = allocations_of(|| {
            for state in &states {
                running.merge_in_place(state.as_ref()).unwrap();
            }
        });
        assert_eq!(
            allocations,
            0,
            "{:?}: {} in-place merges allocate",
            family.0,
            states.len()
        );
    }
}
