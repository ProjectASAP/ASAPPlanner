# Offline frequency measurements, 2026-09-08

Audience: planner developers evaluating offline evidence integration.

Actual ProjectASAP/sketch-bench revision
`87f619e843fd2e4da784160d4e205a0d0d55f032` was built in release mode using
`asap_sketchlib 0.2.2`, RegularPath/Vector2D, with Polars limited to one thread.
All inputs are synthetic: 20,000 i64 keys, key-space 1,000, seed 42. Uniform has
1,000 distinct keys; Zipf (exponent 1.1) has 952. No o11y trace distribution was
measured. Five timed repetitions follow two warmups; each distribution has one
offline accuracy trial. Raw samples and standard deviations are retained.

| Distribution / implementation | Insert CPU ns/item | Read CPU ns/key | Retained requested heap B | Peak requested heap B | Mean absolute relative frequency error |
| --- | ---: | ---: | ---: | ---: | ---: |
| Uniform CMS (272 × 5) | 36.64 | 49.80 | 5,440 | 5,440 | 1.6347 |
| Uniform CountSketch (30,000 × 83) | 1,179.19 | 3,813.40 | 9,960,000 | 9,960,000 | 0 |
| Zipf CMS (272 × 5) | 35.68 | 44.33 | 5,440 | 5,440 | 2.3165 |
| Zipf CountSketch (30,000 × 83) | 948.19 | 3,911.34 | 9,960,000 | 9,960,000 | 0 |

CMS is much cheaper at these formal sizing points, but average relative error
on individual low-frequency keys is high (163.47% / 231.65%). Its formal epsilon
parameter bounds additive error relative to stream mass; it does **not** promise
1% relative error per key. CountSketch's zero observed error is only for these
fixed offline inputs. None of these errors estimates PromQL count_over_time error.

| Exact baseline | Insert CPU ns/item | Prepare CPU ns/dataset | Read CPU ns/key | Prepared state footprint B (upstream formula) |
| --- | ---: | ---: | ---: | ---: |
| Uniform Polars group-by + HashMap | 1.80 | 653,000 | 52.40 | 290,816 |
| Zipf Polars group-by + HashMap | 1.87 | 705,800 | 55.46 | 290,816 |

The exact reference buffers input, groups/counts with Polars, and then serves
point-frequency reads from a HashMap. Its offline error is zero. Prepared state
footprint is taken from the query operation, because upstream flattening preserves
the first operation's memory and would otherwise omit the constructed index.
The exact footprint is formula based; do not label it measured peak heap.

For an illustrative 1,000 point lookups after loading these 20,000 keys, adding
the measured insert/prepare/read components gives 0.783 ms CMS versus 0.741 ms
exact on uniform, and 0.758 ms CMS versus 0.799 ms exact on Zipf. CountSketch
gives 27.397 ms and 22.875 ms respectively. These are **partial CPU component
estimates**, excluding unmeasured empty-sketch construction. They are neither
complete cost estimates nor a demonstrated query-frequency break-even point.
They also show that a smaller sketch is not automatically faster than a prepared
exact index. CPU timings cover the upstream wrapper regions, including their
allocation/destruction overhead; they are not hardware instruction costs.

The companion memory probe uses the identical upstream dataset generator and
release sketch libraries, but a counting System allocator. It measures requested
allocation bytes across construction and insertion while the sketch remains
alive, excludes the input dataset, and asserts every sketch allocation is released
after destruction. It does not measure allocator-resident pages or jemalloc
overhead. Process RSS remains in the raw reports. Disk usage and serialized
state size were not measured and remain null.

Files `manifest.json`, `operation-reports.json`, `raw.json`, `memory-probe.json`,
`planner-evidence.json`, and `exact-baselines.json` preserve the evidence and
provenance. `context-uniform.json` and `context-zipf.json` explicitly select the
synthetic applicability contexts; they must not be represented as actual o11y
production distribution profiles.
