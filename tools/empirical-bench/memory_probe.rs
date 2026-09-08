//! Counts requested allocation bytes with each sketch alive; input generation is excluded.
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use asap_sketchlib::{Count, CountMin, DataInput, RegularPath, Vector2D};

struct Tracking;
static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);
fn account(bytes: usize) {
    let live = LIVE.fetch_add(bytes, Relaxed) + bytes;
    PEAK.fetch_max(live, Relaxed);
}
unsafe impl GlobalAlloc for Tracking {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() { account(layout.size()); }
        ptr
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc_zeroed(layout) };
        if !ptr.is_null() { account(layout.size()); }
        ptr
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) };
        LIVE.fetch_sub(layout.size(), Relaxed);
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        let next = unsafe { System.realloc(ptr, layout, size) };
        if !next.is_null() {
            if size >= layout.size() { account(size - layout.size()); }
            else { LIVE.fetch_sub(layout.size() - size, Relaxed); }
        }
        next
    }
}
#[global_allocator]
static ALLOCATOR: Tracking = Tracking;

fn main() {
    let path = std::env::args().nth(1).expect("raw.json path");
    let rows: Vec<serde_json::Value> = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    let mut results = Vec::new();
    for row in rows {
        let description: aqpbm_datagen::TableDescription = serde_json::from_value(row["workload"]["synthetic"]["description"].clone()).unwrap();
        let items = description.generate().unwrap().into_column(0).unwrap().into_i64().unwrap();
        if row["impl"] == "polars" {
            use sketch_bench::wrappers::polars_shared::PolarsFrequencyCore;
            // Initialize Polars' worker/cache state before attributing bytes to one live index.
            for _ in 0..2 {
                let mut warm = PolarsFrequencyCore::<i64>::default();
                for item in &items { warm.update(item); }
                warm.finalize();
            }
            let mut samples = Vec::with_capacity(5);
            let mut cleanup_deltas = Vec::with_capacity(5);
            for _ in 0..5 {
                let baseline = LIVE.load(Relaxed);
                PEAK.store(baseline, Relaxed);
                let mut state = PolarsFrequencyCore::<i64>::default();
                for item in &items { state.update(item); }
                state.finalize();
                std::hint::black_box(&state);
                let sample = (LIVE.load(Relaxed).saturating_sub(baseline), PEAK.load(Relaxed).saturating_sub(baseline));
                drop(state);
                cleanup_deltas.push(LIVE.load(Relaxed) as i64 - baseline as i64);
                samples.push(sample);
            }
            results.push(serde_json::json!({"sketch": row["sketch"], "impl": "polars",
                "workload": row["workload"], "sketch_config": row["sketch_config"], "samples": samples,
                "cleanup_deltas": cleanup_deltas,
                "method": "requested allocation bytes using counting System allocator; exact upstream PolarsFrequencyCore construction + insertion + prepare; state alive; input and two startup warmups excluded; valid only when cleanup deltas are zero; not allocator-resident pages"}));
            continue;
        }
        let depth = row["sketch_config"]["params"]["rows"].as_u64().unwrap() as usize;
        let width = row["sketch_config"]["params"]["cols"].as_u64().unwrap() as usize;
        let cms = row["sketch"].as_str().unwrap().starts_with("cms-");
        let mut samples = Vec::with_capacity(5);
        for _ in 0..5 {
            let baseline = LIVE.load(Relaxed);
            PEAK.store(baseline, Relaxed);
            // The sketch remains alive at both snapshots, unlike a consuming benchmark closure.
            let (retained, peak) = if cms {
                let mut sketch = CountMin::<Vector2D<i32>, RegularPath>::with_dimensions(depth, width);
                for item in &items { sketch.insert(&DataInput::I64(*item)); }
                std::hint::black_box(&sketch);
                (LIVE.load(Relaxed) - baseline, PEAK.load(Relaxed) - baseline)
            } else {
                let mut sketch = Count::<Vector2D<i32>, RegularPath>::with_dimensions(depth, width);
                for item in &items { sketch.insert(&DataInput::I64(*item)); }
                std::hint::black_box(&sketch);
                (LIVE.load(Relaxed) - baseline, PEAK.load(Relaxed) - baseline)
            };
            assert_eq!(LIVE.load(Relaxed), baseline, "sketch destruction must release its allocations");
            samples.push((retained, peak));
        }
        results.push(serde_json::json!({"sketch": row["sketch"], "impl": "lib", "workload": row["workload"], "sketch_config": row["sketch_config"],
            "samples": samples, "method": "requested allocation bytes using counting System allocator; construction + insertion; sketch alive; dataset excluded; not allocator-resident pages"}));
    }
    println!("{}", serde_json::to_string_pretty(&results).unwrap());
}
