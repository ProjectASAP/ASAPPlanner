//! Complements sketch-bench with empty construction CPU and actual persisted snapshot size.
use std::hint::black_box;
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use asap_sketchlib::{Count, CountMin, DataInput, RegularPath, Vector2D};

#[global_allocator]
static ALLOCATOR: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

fn cpu_ns() -> u64 {
    #[repr(C)]
    struct Timespec { tv_sec: std::os::raw::c_long, tv_nsec: std::os::raw::c_long }
    extern "C" { fn clock_gettime(clock_id: std::os::raw::c_int, time: *mut Timespec) -> std::os::raw::c_int; }
    // Linux CLOCK_PROCESS_CPUTIME_ID; this companion also uses Linux allocated blocks.
    assert!(cfg!(target_os = "linux"));
    let mut time = Timespec { tv_sec: 0, tv_nsec: 0 };
    assert_eq!(unsafe { clock_gettime(2, &mut time) }, 0);
    time.tv_sec as u64 * 1_000_000_000 + time.tv_nsec as u64
}

fn construction<T>(mut build: impl FnMut() -> T, batch: usize) -> Vec<f64> {
    let mut samples = Vec::with_capacity(5);
    for run in 0..7 {
        let mut alive = Vec::with_capacity(batch);
        let start = cpu_ns();
        for _ in 0..batch { alive.push(black_box(build())); }
        let elapsed = cpu_ns() - start;
        if run >= 2 { samples.push(elapsed as f64 / batch as f64); }
        // Empty objects remain alive until after the timed construction batch.
        black_box(&alive);
        drop(alive);
    }
    samples
}

fn live_phases<T>(items: &[i64], build: impl Fn() -> T, insert: impl Fn(&mut T, i64),
                  read: impl Fn(&T, i64) -> f64, prepare: impl Fn(&mut T),
                  merge: Option<fn(&mut T, &T)>) -> serde_json::Value {
    let mut keys: Vec<_> = items.iter().copied().collect::<std::collections::BTreeSet<_>>().into_iter().collect();
    // Fixed rotation avoids sorted probe order without introducing another RNG definition.
    if !keys.is_empty() { let mid = keys.len() / 2; keys.rotate_left(mid); }
    let mut updates = Vec::with_capacity(5);
    let mut prepares = Vec::with_capacity(5);
    let mut reads = Vec::with_capacity(5);
    let mut merges = Vec::with_capacity(5);
    for run in 0..7 {
        let mut state = build();
        let start = cpu_ns();
        for item in items { insert(&mut state, black_box(*item)); }
        let update = (cpu_ns() - start) as f64 / items.len() as f64;
        let start = cpu_ns();
        prepare(&mut state);
        let prepare = (cpu_ns() - start) as f64;
        let start = cpu_ns();
        for _ in 0..10 { for key in &keys { black_box(read(&state, black_box(*key))); } }
        let query = (cpu_ns() - start) as f64 / (keys.len() * 10) as f64;
        let merge_time = merge.map(|merge| {
            let mut other = build();
            for item in items { insert(&mut other, *item); }
            let start = cpu_ns();
            merge(&mut state, &other);
            (cpu_ns() - start) as f64
        });
        if run >= 2 {
            updates.push(update); prepares.push(prepare); reads.push(query);
            if let Some(time) = merge_time { merges.push(time); }
        }
        black_box(&state);
    }
    serde_json::json!({"update_cpu_ns_samples": updates, "prepare_cpu_ns_samples": prepares,
        "read_cpu_ns_samples": reads, "merge_cpu_ns_samples": merges,
        "query_distinct_keys": keys.len(), "query_passes": 10,
        "method": "CLOCK_PROCESS_CPUTIME_ID; live state with constructor/destructor excluded; five runs after two warmups; update per input item; read per all-distinct-key lookup across ten repeated fixed-snapshot probe passes; merge per binary merge; fixed rotated sorted key order"})
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let rows: Vec<serde_json::Value> = serde_json::from_slice(&std::fs::read(&args[1]).unwrap()).unwrap();
    let directory = std::path::Path::new(&args[2]);
    let mut results = Vec::new();
    for (index, row) in rows.into_iter().enumerate() {
        let description: aqpbm_datagen::TableDescription = serde_json::from_value(row["workload"]["synthetic"]["description"].clone()).unwrap();
        let items = description.generate().unwrap().into_column(0).unwrap().into_i64().unwrap();
        if row["impl"] == "polars" {
            use sketch_bench::wrappers::polars_shared::PolarsFrequencyCore;
            let samples = construction(PolarsFrequencyCore::<i64>::default, 4096);
            let phases = live_phases(&items, PolarsFrequencyCore::<i64>::default,
                |s, x| s.update(&x), |s, x| s.query(&x) as f64, |s| s.finalize(), None);
            results.push(serde_json::json!({"sketch": row["sketch"], "impl": row["impl"],
                "workload": row["workload"], "sketch_config": row["sketch_config"],
                "build_cpu_ns_samples": samples, "build_batch": 4096,
                "phases": phases,
                "build_method": "process CPU around upstream PolarsFrequencyCore::<i64>::default(); output holder allocation and destruction excluded"}));
            continue;
        }
        let depth = row["sketch_config"]["params"]["rows"].as_u64().unwrap() as usize;
        let width = row["sketch_config"]["params"]["cols"].as_u64().unwrap() as usize;
        let batch = (16_000_000 / (depth * width * 4)).clamp(2, 256);
        let (samples, serialized, phases) = if row["sketch"].as_str().unwrap().starts_with("cms-") {
            let samples = construction(|| CountMin::<Vector2D<i32>, RegularPath>::with_dimensions(depth, width), batch);
            let phases = live_phases(&items, || CountMin::<Vector2D<i32>, RegularPath>::with_dimensions(depth, width),
                |s,x| s.insert(&DataInput::I64(x)), |s,x| s.estimate(&DataInput::I64(x)) as f64, |_| {}, Some(CountMin::<Vector2D<i32>, RegularPath>::merge));
            let mut sketch = CountMin::<Vector2D<i32>, RegularPath>::with_dimensions(depth, width);
            for item in &items { sketch.insert(&DataInput::I64(*item)); }
            let bytes = sketch.serialize_to_bytes().unwrap();
            let restored = CountMin::<Vector2D<i32>, RegularPath>::deserialize_from_bytes(&bytes).unwrap();
            for item in &items { assert_eq!(sketch.estimate(&DataInput::I64(*item)), restored.estimate(&DataInput::I64(*item))); }
            (samples, bytes, phases)
        } else {
            let samples = construction(|| Count::<Vector2D<i32>, RegularPath>::with_dimensions(depth, width), batch);
            let phases = live_phases(&items, || Count::<Vector2D<i32>, RegularPath>::with_dimensions(depth, width),
                |s,x| s.insert(&DataInput::I64(x)), |s,x| s.estimate(&DataInput::I64(x)), |_| {}, Some(Count::<Vector2D<i32>, RegularPath>::merge));
            let mut sketch = Count::<Vector2D<i32>, RegularPath>::with_dimensions(depth, width);
            for item in &items { sketch.insert(&DataInput::I64(*item)); }
            let bytes = sketch.serialize_to_bytes().unwrap();
            let restored = Count::<Vector2D<i32>, RegularPath>::deserialize_from_bytes(&bytes).unwrap();
            for item in &items { assert_eq!(sketch.estimate(&DataInput::I64(*item)), restored.estimate(&DataInput::I64(*item))); }
            (samples, bytes, phases)
        };
        let path = directory.join(format!("snapshot-{index}.msgpack"));
        let mut file = std::fs::OpenOptions::new().write(true).create_new(true).open(&path).unwrap();
        file.write_all(&serialized).unwrap();
        file.sync_all().unwrap();
        let metadata = file.metadata().unwrap();
        assert_eq!(metadata.len(), serialized.len() as u64);
        let disk_bytes = metadata.blocks() * 512;
        drop(file);
        std::fs::remove_file(path).unwrap();
        results.push(serde_json::json!({"sketch": row["sketch"], "impl": row["impl"],
            "workload": row["workload"], "sketch_config": row["sketch_config"],
            "build_cpu_ns_samples": samples, "build_batch": batch,
            "phases": phases,
            "build_method": "CLOCK_PROCESS_CPUTIME_ID around empty sketch constructors in batched live objects; holder allocation and all destruction excluded; jemalloc; five runs after two warmups",
            "serialized_bytes": serialized.len(), "disk_bytes": disk_bytes,
            "disk_method": "actual flushed single MsgPack snapshot file allocated blocks*512 on local temporary filesystem; directory/inode overhead excluded; file removed after measurement; not runtime persistence demand"}));
    }
    println!("{}", serde_json::to_string_pretty(&results).unwrap());
}
