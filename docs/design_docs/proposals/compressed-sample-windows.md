# Compressed sample windows

**Status:** Proposed; design only, no code yet.

**Audience:** Planner designers and implementers, and implementers of a
deployment's data plane.

A tumbling-window chain can keep, per series and window, a summary (a sketch)
or an exact accumulator (sum, count, max, counter). This proposal adds a third
window state: **compressed samples**, every sample of the series in the window,
encoded the way a time-series database stores them (delta-of-delta timestamps
and XOR values). At query time a new operator decodes the windows a range
covers back into raw sample rows, and the existing exact operators answer the
query from them. The answer is exact. Stage 3 chooses between compressed
samples, sketches and exact accumulators by cost, as it chooses any candidate.

Related designs: [multi-level-window-chains.md](multi-level-window-chains.md)
(wider chain levels and range covers),
[window-materialization-choices.md](window-materialization-choices.md) (window
semantics vs implementation vs materialization), and the deployment
capabilities in `asap-types` (`DeploymentCapabilities`).

## 1. Problem, with a measured example

Two dashboard workloads, 10,000 series per metric, planned with raw data not
retained (`raw_data_retained: false`) and a cluster memory limit equal to what
a compressed-sample time-series database baseline used on the same workload:

| Workload | Queries (all `rate`/`increase` over the counter `data_total`, the rest over the gauge `data`) | Rate | Least memory any plan needs | Limit |
| --- | --- | --- | --- | --- |
| dashboard-heavy | `quantile_over_time(0.99, data[1h])` every 10 s; `topk(10, sum by (…) (rate(data_total[1h])))` every 15 s; `max_over_time(data[30m])` every 10 s; `sum by (…) (increase(data_total[1h]))` every 15 s | 10 samples/s | 5.10 GB | 3.7 GB |
| typical-dashboard | 11 panels: `rate`, `increase` over 5 min–1 h; `avg_over_time` 5 min, 30 min; `max_over_time` 15 min, 1 h; `quantile_over_time` 0.99 over 15 min and 0.5 over 1 h; two `topk`; refresh 10–30 s | 1 sample/s | 3.72 GB | 0.5 GB |

Plan selection fails on both: `no plan fits the cluster's limits: memory: the
least any plan needs is 5096909217 bytes, the limit is 3700000000 bytes`, and
`… 3718300131 bytes, the limit is 500000000 bytes`.

Where the bytes go in the least-memory plan (cost model 1, `keeps … bytes` of
each chain in `asap_planner --format text`):

| Workload | Chain | Window | Bytes per window row | Kept |
| --- | --- | --- | --- | --- |
| dashboard-heavy | KLL over `data` (1 h quantile) | 10 s (100 samples) | 1,136 | 4.10 GB |
| | exact Max over `data` | 10 s | 81 | 0.15 GB |
| | exact Counter over `data_total`, two chains | 15 s | 153–169 | 0.78 GB |
| typical-dashboard | DDSketch over `data` (1 h median) | 30 s (30 samples) | 1,680 | 2.03 GB |
| | DDSketch over `data` (15 min p99) | 20 s (20 samples) | 1,640 | 0.75 GB |
| | exact Sum, Count, Max over `data`, six chains | 10–30 s | 96–128 | 0.39 GB |
| | exact Counter over `data_total`, four chains | 5–30 s | 137–153 | 0.49 GB |

A DDSketch window over 30 samples takes 1,680 bytes, 56 bytes per sample it
summarizes. The same 30 samples, compressed, take 30–220 bytes
(section 3.3). A KLL window over 100 samples takes 1,136 bytes.

What exists and why it does not help:

- **Exact candidates read raw rows.** `quantile_over_time` over raw rows is
  exact, but its input group `TimeRange(1h) ← Scan` has no buildable producer
  when raw data is not retained: the deployment excludes the query-time `Scan`
  and raw rows kept across evaluations (`ExclusionSource::RawDataNotRetained`;
  the plan text lists `ExcludedByDeployment(raw_data_not_retained) 28`).
- **Raw rows kept across evaluations** (a `Scan` with
  `Lifetime::AcrossEvaluations`), where allowed, are priced uncompressed at
  `row_bytes`: 48 bytes per sample for a series identity, a label, a time and
  a value.
- **Exact accumulators** cover only `Sum`, `Count`, `Min`, `Max`, `Counter`
  and `IRate` (`ExactKind`). A quantile or a top-k over values has no exact
  window state, so only sketch chains answer it.
- **One chain per state type.** Each query's chain holds its own state: the
  `data` gauge in typical-dashboard is kept by eight chains (Sum, Count and Max
  twice, DDSketch twice), each over the same samples.

So the candidate space has no state that is both exact and small for windows
of few samples, and nothing that one chain could serve to every reader of a
metric.

## 2. Components

### 2.1 State type: compressed samples

`FieldDataType::CompressedSamples` is a window state holding every sample of
one series in one window, in timestamp order, encoded as one block:

| Part | Encoding | Bytes |
| --- | --- | --- |
| Header | sample count `u32`, first timestamp `i64` (ms), first value (`f64` bits), payload length in bits `u32` | 24 |
| Timestamps after the first | delta of delta: `0` → 1 bit; within [−63, 64] → 2 + 7 bits; [−255, 256] → 3 + 9; [−2047, 2048] → 4 + 12; else 4 + 32 | 1 bit each at a regular interval |
| Values after the first | XOR with the previous value's bits: equal → 1 bit; meaningful bits inside the previous leading/trailing-zero window → 2 + that width; else 2 + 5 (leading zeros) + 6 (width) + width | 1–77 bits each |

This is the encoding of the Gorilla paper (VLDB 2015), which time-series
databases use for in-memory blocks. It is lossless on the `f64` bit pattern:
NaN payloads (PromQL stale markers), ±0 and ±∞ come back bit for bit.

There is one encoding and no encoding parameter: the state has no θ, so a
chain group of it has one build.

**Which queries it answers.** Decoded, a window is the series' raw samples in
it, so every function over raw samples answers exactly from it, through the
operators that answer it from raw rows today:

| Function | Read |
| --- | --- |
| `quantile_over_time`, `max_over_time`, `min_over_time` | the range's values |
| `sum_over_time`, `count_over_time`, `avg_over_time`, `stddev_over_time`, `stdvar_over_time` | the range's values |
| `rate`, `increase`, `delta`, `irate`, `idelta`, `resets`, `changes`, `deriv` | the range's samples with their timestamps (reset handling and extrapolation as for raw samples) |
| `last_over_time`, an instant selector | the newest sample at or before `T` within the lookback |
| `topk`/`bottomk` over values, and any aggregation across series above these | the per-series results, unchanged |

**Merge.** Two blocks of one series over disjoint time spans merge by
concatenation in time order; blocks whose spans overlap (two ingest shards
building one window of a series) merge by a sorted merge of their samples and
one re-encode. Either way the result is exact. No plan in this design merges
across windows (section 3.2); the merge is the kernel's `merge_with` for
shard-parallel builds and for persistence compaction.

### 2.2 Build and seal at ingestion

The chain `SummaryTumblingWindowChain { summary_type: CompressedSamples,
input: value, reduction: PerEntity, tumbling_window_width_ms: w }` over a scan
of the metric builds one block per series per window, as every chain builds its
states. The input row is `(series identity, timestamp, value)`; the window time
index is the chain's.

- **Build:** each sample is appended to its series' open block, bit by bit.
  A sample older than the newest one in the open block (out of order within the
  window) re-encodes the open block with the sample inserted, so the block stays
  exact; its cost is charged only when the data workload declares out-of-order
  arrival.
- **Seal:** at the window's end (plus the deployment's allowed lateness, as for
  every chain) the block is finished and shrunk to its encoded length.

### 2.3 Read at query time: `DecodeSamplesInRange`

A new ASAP operator reads a compressed-samples chain and returns the raw sample
rows of one range, `(T − offset − range, T − offset]`, per series:

1. Select the chain's windows that **overlap** the range, including the window
   still open at `T` (its samples so far).
2. Decode each window's block in time order.
3. Keep only the samples inside the range (for an instant read: the newest
   sample at or before `T − offset` within the lookback).
4. Derive the label columns the reader needs from each series' identity, and
   emit rows series-major, in bounded batches.

Its output has exactly the schema and rows of `TimeRange(range) ← Scan` over the
same metric. It is a candidate of that group, so every exact consumer of raw
rows (the `Aggregate` of `quantile_over_time`, `max_over_time`, `rate`, …)
reads it unchanged.

Because it trims both edges and reads the open window, the chain's width does
not have to divide the readers' evaluation grid, range or offset. One chain per
metric serves every reader of the metric.

### 2.4 Kernel home: `asap_sketchlib`

The encoder, decoder, merge and portable byte format live in `asap_sketchlib`
as `CompressedSamples`, next to the sketches whose portable formats and golden
vectors it already hosts. The executor wraps it as it wraps every sketch
(`summary_kernels/`). It is not a `SketchAlgorithm` (it has no error and no θ),
so it is a separate type, not a variant of that enum. `asap_sketchlib` has no
sample codec today (no Gorilla, XOR or delta-of-delta code under `src/`).

### 2.5 Planner vocabulary

- `FieldDataType::CompressedSamples` and `SummaryFamily::CompressedSamples`
  (JSON `"compressed_samples"`), so a deployment can exclude it:
  `{"summary_family": "compressed_samples"}`.
- `ASAPOp::DecodeSamplesInRange` and `OperatorKind::DecodeSamplesInRange`, so a
  deployment whose runtime lacks the operator excludes
  `{"operator": "DecodeSamplesInRange"}`.
- A data fact per source column, `column_compressed_bits_per_sample`, and the
  assumption recorded when it is missing,
  `DataFactAssumption::CompressedBitsPerSample` (cost only).

## 3. Algorithm

### 3.1 Pass 2: one compressed-samples chain per metric

New Pass 2 rule `add_compressed_sample_chains`, run after `add_chain_groups`:

```
for each group R = TimeRange(W, kind) ← TimeShift*(Scan(source, columns)):
    if source is not a series source with one numeric value column: skip
    X  = the memo group of Scan(source, [time, value, series identity])
    for w in widths(source):                       # see below
        C_w = group SummaryTumblingWindowChain[CompressedSamples, w](X)
    add to R, for each C_w: DecodeSamplesInRange { child: C_w, range: W,
        kind, labels: columns R carries besides time, value and identity }
```

- **Series sources.** A PromQL metric (series identity, time, value), or a SQL
  table whose unique key is (series key columns, time index) read for one
  numeric column. Other tables get no candidate in this design.
- **Keyed by identity only.** The chain is built over a scan of time, value and
  series identity, so the scans that differ only in the labels they project
  (typical-dashboard has three `Scan` groups of `data`, projecting `job`,
  `region`, and `region, service`) share one chain. The labels are a function
  of the identity; the decode derives them.
- **Widths.** The ladder of
  [multi-level-window-chains.md](multi-level-window-chains.md) — 1 min, 5 min,
  15 min — limited to widths no longer than the metric's longest reader range.
  The grid does not constrain them (section 2.3). Stage 3 picks one by cost:
  a wider window holds fewer per-window bytes, but its readers decode on
  average half a window of samples outside their range, and the chain keeps one
  more window.
- **Edges.** The edge `DecodeSamplesInRange → C_w` is exempt from
  `ChainWidthDoesNotDivideEvaluationGrid`, and the chain's delivery check in
  Stage 3 treats the decode's read as reaching the open window.

Example (typical-dashboard): groups `TimeRange 300s/900s/1800s/3600s ← Scan`
over `data` (five groups) and over `data_total` (four) each gain three decode
candidates; two metrics × three widths = six new chain groups.

**Complexity.** One chain group per (metric, width): at most 3 per metric, and
one decode candidate per raw-range group and width. The MILP gains those nodes
and edges; no new constraint family.

### 3.2 Interaction with chains, time ranges and multi-level covers

- **Not a level of a cover.** A compressed-samples window is never merged into
  a wider window: concatenation keeps every sample, so a wider level holds the
  same bytes (less only the per-window bytes) and a query decodes the same
  samples. `add_wider_levels` therefore skips this state type, and no
  `TimeRangeCover` has compressed-samples levels. Decode work is per sample in
  the range, not per window.
- **Finest level only, in effect.** The chain is the finest store of a metric;
  coarser levels, where a plan wants them, are sketch or exact-accumulator
  chains built from the arriving samples as today, not from decoded windows.
- **No `TimeRange` over it.** `DecodeSamplesInRange` selects its windows itself
  (overlap, then trim), so the `TimeRange`-over-chain rule (windows ending in
  `(T − W, T]`) does not apply to this state.
- **Deferred: mixed covers.** A cover whose interior is wide sketch windows
  and whose edges are decoded samples (folded into the sketch) would cut the
  narrow sketch level's memory. It needs a cover over levels of different state
  types; it is a follow-up, not part of this design.

### 3.3 Cost model

Per window state (`summary_state_bytes`, from `StateInputRows::rows_per_state`
= samples per series per window `n`, and `b` = the column's bits per sample):

```
state_bytes(n, b)  = 24 + ceil(n · b / 8)                # header + payload
window_row_bytes   = state_bytes + 8 (blob offset) + state_row_bytes_beside_state
                   = state_bytes + 8 + 72               # identity 64, window end 8
```

`b` is `column_compressed_bits_per_sample[value]` when the source declares it,
else `ASSUMED_COMPRESSED_BITS_PER_SAMPLE = 72` (9 bytes), recorded as
`DataFactAssumption::CompressedBitsPerSample`. 72 bits covers i.i.d. random
doubles (measured 58 bits) plus jittered timestamps (9 bits); declared data does
better.

Measured on the workloads' own generators (a reference codec, one core of a
2.0 GHz x86 server, 2,000 series × 3,600 samples, bitwise round trip checked):

| Data | Bits/sample (steady state) | Bytes/sample incl. header | Encode | Decode |
| --- | --- | --- | --- | --- |
| `data`: rank-scaled Pareto(2) gauge, i.i.d. per sample | 58.2 (57.1 value + 1.0 time) | 7.37 | 25 ns | 9.6 ns |
| `data_total`: counter `(n + 1) · scale`, non-integer scale | 56.8 | 7.26 | 23 ns | 10.1 ns |
| integer counter `n + 1` | 9.7 | 1.50 | 20 ns | 9.5 ns |
| 2-decimal slowly changing gauge | 6.7 | — | — | — |
| scrape jitter ±5 ms (Pareto gauge) | 65.7 | — | — | — |

Both benchmark metrics are close to the worst case for XOR encoding: every
sample is a fresh random double. Production metrics compress to about 1.37
bytes per sample (the Gorilla paper's average).

The other terms:

| Term | Formula | Charged on |
| --- | --- | --- |
| Kept memory | `window_row_bytes × (ceil(W_max / w) + 1) × series`, `W_max` the longest reader range | the chain, `AcrossEvaluations(Memory)`, as every chain |
| Ingestion CPU | `encode_ns × samples/s` (25 ns analytical until measured; DDSketch's chain build is 340 ns per row, measured) | the chain |
| Decode CPU per evaluation | `decode_ns × (W + w/2) · rate × series` (10 ns analytical until measured) | `DecodeSamplesInRange` |
| Exact evaluation | the existing operator costs over the decoded rows | the consumers, unchanged |
| Query-time memory | one series' decoded rows per query thread (rows stream series-major) | `DecodeSamplesInRange` |
| Latency | decode plus evaluation of one evaluation, as for every query-time node | cost model 3 (`query_ms_per_step`) |

A range request (`range_evaluation`) decodes `(T₀ − W, Tₙ]` once for all its
steps, as a raw scan does, and the exact operators evaluate each step.

### 3.4 Stage 3: the trade-off

Stage 3 needs no new constraint or objective term. The MILP sees, per raw-range
group, the decode candidates beside the sketch and exact-accumulator chains, and
the chain once per metric however many readers share it.

- **Under a memory limit**, one compressed chain per metric replaces several
  per-query chains: 0.30 GB instead of 3.17 GB for `data` in typical-dashboard
  (section 4).
- **Against latency**, decode is per sample: `quantile_over_time(0.99,
  data[1h])` at 10 samples/s decodes 363 million samples per evaluation, 3.6
  core-seconds at 10 ns. The 10 s KLL chain merges 3.6 million states, 12.4 s
  (model); a multi-level KLL cover merges about 12 per series, far less. So
  without a memory limit Stage 3 may keep sketches with covers for long
  high-rate ranges, and pick compressed samples for low rates and short ranges.
- **Against accuracy**, nothing: the decode is exact, so every accuracy target,
  including `AccuracyTarget::exact()`, is met (section 3.5).

### 3.5 Accuracy

`DecodeSamplesInRange` returns the same rows as `TimeRange ← Scan`, so its
guarantee is exact and the consumers' guarantees are those of the exact plan.
The accuracy crate gives the operator `AccuracyGuarantee` exact. Two conditions
are shared with every chain and the raw scan, not new:

- A sample later than its window's end plus the allowed lateness is not in the
  sealed window; with `InOrderArrival` assumed, as today.
- A sample that has not arrived by `T` is not in the answer, as for a raw scan.

### 3.6 Executor

| Step | Operator / kernel | Reuse |
| --- | --- | --- |
| Build | the chain operator with a `CompressedSamples` kernel; a state column stores one byte blob per state with offsets (`StateStorage::CompressedSamples`), no allocation per state | chain build, refresh and retention unchanged |
| Seal | finish the block, shrink to its encoded length | chain seal |
| Merge | concatenate disjoint blocks; sorted merge and re-encode for overlapping ones (`AggregateCore::merge_with`) | |
| Read | `DecodeSamplesInRange`: request the windows overlapping the range plus the open window's snapshot (its blocks up to the last appended sample), decode, trim, derive labels, emit series-major batches | the materialized-view read, widened by one window |
| Evaluate | none new: the decoded rows go to the raw-row operators (`range_function`, `aggregate`, …) | all exact evaluation |

The open-window snapshot is the one runtime extension: the builder publishes
each open block's (sample count, bit length) after each appended batch, and the
reader copies the bytes up to it. Range functions over series-major input free
each series' rows after evaluating it, so query memory is bounded by one series
per thread.

### 3.7 Relation to `raw_data_retained`

| | `raw_data_retained: true` | Compressed-sample chain |
| --- | --- | --- |
| What it is | a deployment fact: the data plane keeps a raw store that query-time `Scan`s read, and stored data (`AtRest`, `Mixed`) lives in it | a plan choice: a state the plan builds and keeps, like any chain |
| Who chooses retention | the deployment | Stage 3 ($h_v$ = the longest reader's range plus one window) |
| Priced | raw rows at `row_bytes` (48 bytes/sample) | `window_row_bytes` (about 1–9 bytes/sample plus per-window bytes) |
| Allowed when raw data is not retained | no | yes |

It does not replace the flag. The flag also states whether historical data
exists to scan (`DeploymentCapabilities::validate` rejects stored data without
it), and a deployment with a raw store keeps its query-time `Scan` candidates.
A deployment that must not keep raw samples at all (for example by policy)
excludes `{"summary_family": "compressed_samples"}`.

### 3.8 Persistence and tiered storage

A sealed window's block is its own portable byte format, the same bytes in the
heap, in a file and in the page cache. That fits the data plane's tiered view
storage (designed separately in the data plane), in its columnar fragment form:
per (view, window), a text-index column for series identities, a plain column
for window ends, and a states column of byte blobs with offsets. A compressed
block is one such blob, stored verbatim:

- **No state decode on a cold read.** A sketch must be deserialized from its
  file bytes before use (about 3 µs per state measured for the row form);
  compressed samples are decoded straight from the page-cache bytes, the same
  decode the query pays anyway.
- **Bytes priced = bytes stored.** The cost model's `state_bytes` is the file
  size, so the tiered design's "encoded bytes per state" requirement is exact
  for this state.
- **Format version.** The block header carries no version; the fragment's
  format version covers it, and `asap_sketchlib` keeps golden vectors for it.

Placing chains in a file tier is a Stage 2 choice (`StorageMedium::Disk`), to
be designed with that deployment capability; this design keeps the chain in
memory and prices it there.

## 4. Predicted memory at 10,000 series

From the section 3.3 formulas, with the least-memory plan's other chains
(section 1) where they are smaller. `b` is per metric (`data` / `data_total`).

**typical-dashboard** (1 sample/s, longest range 1 h for both metrics; today
3.17 GB of `data` chains + 0.49 GB of `data_total` chains = 3.66 GB, 3.72 GB
peak; limit 0.5 GB):

| `b` (bytes/sample) | w = 1 min | w = 5 min | w = 15 min | Fits 0.5 GB |
| --- | --- | --- | --- | --- |
| measured on the benchmark's generator: 7.27 / 7.10 | 0.33 + 0.32 = 0.65 GB | 0.30 + 0.29 = **0.59 GB** | 0.33 + 0.33 = 0.66 GB | no (6.2× below today) |
| assumed default, nothing declared: 9 / 9 | 0.39 + 0.39 = 0.79 GB | 0.36 + 0.36 = 0.73 GB | — | no |
| production-like: 1.37 / 1.21 | 0.11 + 0.11 = 0.22 GB | 0.07 + 0.06 = **0.13 GB** | — | yes |

Plus about 0.06 GB of query-time memory (today's peak over average). With the
benchmark's i.i.d. random values typical-dashboard still does not fit 0.5 GB in
memory; it fits with the chain's windows in a file tier (section 3.8).

**dashboard-heavy** (10 samples/s, longest range 1 h; today `data` 4.25 GB
(KLL + Max) and `data_total` 0.78 GB (counters), 5.10 GB peak; limit 3.7 GB):

| `b` | `data` compressed, w = 1 min | `data_total` | Total kept | Fits 3.7 GB |
| --- | --- | --- | --- | --- |
| measured: 7.27 / 7.10 | 2.72 GB | counters kept, 0.78 GB (compressed would be 2.66 GB) | **3.50 GB**, about 3.58 GB peak | yes, by 0.12 GB |
| assumed default: 9 / 9 | 3.36 GB | counters, 0.78 GB | 4.13 GB | no |
| production-like: 1.37 / 1.21 | 0.56 GB | compressed, 0.51 GB | **1.07 GB** | yes |

So the benchmark workloads should declare their bits per sample (measured
above); the assumed default is too pessimistic to fit dashboard-heavy.

Query-time CPU (decode only, 10 ns/sample): typical-dashboard 0.086
core-seconds per second over its 11 readers (each 1 h reader 0.38 s per
evaluation, versus 2.03 s for today's 1 h DDSketch merge); dashboard-heavy
0.55 core-seconds per second (the 1 h quantile 3.6 s per evaluation, versus
12.4 s for today's KLL merge). Ingestion: 25 ns per sample, 0.5 ms/s for
typical-dashboard and 5 ms/s for dashboard-heavy.

## 5. Code interface

```rust
// asap-types: ir/schema/mod.rs
pub enum FieldDataType {
    // ... existing variants ...
    /// Every sample of one series in one window, compressed (delta-of-delta
    /// timestamps, XOR values). Exact; no parameters.
    CompressedSamples,
}

// asap-types: summary_type/family.rs
pub enum SummaryFamily {
    Sketch(SketchAlgorithm),
    Exact(ExactKind),
    /// JSON `"compressed_samples"`.
    CompressedSamples,
}

// asap-types: ir/operator/asap.rs
pub enum ASAPOp<C> {
    // ... existing variants ...
    /// The raw sample rows of `(T − range, T]` (or the newest sample within
    /// `range` for `Instant`) per series, decoded from the windows of the
    /// compressed-samples chain `child` that overlap it, the open window
    /// included. Same schema and rows as `TimeRange(range) ← Scan`;
    /// `labels` are derived from each series' identity.
    DecodeSamplesInRange { child: C, range: Duration, kind: TimeRangeKind, labels: Vec<String> },
}

// asap-types: ir/operator/kind.rs
pub enum OperatorKind { /* ... */ DecodeSamplesInRange }

// asap-types: workload/data.rs
pub struct SourceFacts {
    /// Bits one sample of each column takes compressed, keyed by column
    /// (default: none declared), e.g. `{"value": 58.2}`.
    pub column_compressed_bits_per_sample: BTreeMap<String, f64>,
}

// asap-types: workload/assumed_data_fact.rs
pub enum DataFactAssumption {
    /// No `column_compressed_bits_per_sample` for `column`: its compressed
    /// samples are costed at `assumed_bits_per_sample`. Cost only.
    CompressedBitsPerSample { column: String, assumed_bits_per_sample: f64 },
}

// asap-cost: node/state_bytes/mod.rs
pub const ASSUMED_COMPRESSED_BITS_PER_SAMPLE: f64 = 72.0;
pub fn compressed_samples_state_bytes(samples: u64, bits_per_sample: f64) -> u64;

// asap-stage1-pass2-reuse-candidates: rules/compressed_samples.rs
pub(crate) fn add_compressed_sample_chains(memo: &mut Memo, /* ... */);

// asap_sketchlib: compressed_samples.rs
pub struct CompressedSamples { /* header + bit payload */ }
impl CompressedSamples {
    pub fn new() -> Self;
    /// Appends a sample; one older than the newest re-encodes the block.
    pub fn push(&mut self, timestamp_ms: i64, value: f64);
    pub fn len(&self) -> u32;
    pub fn encoded_bits(&self) -> u64;
    /// Samples with `start_exclusive_ms < t <= end_inclusive_ms`, in time order.
    pub fn samples_in(&self, start_exclusive_ms: i64, end_inclusive_ms: i64) -> impl Iterator<Item = (i64, f64)> + '_;
    /// Concatenation for disjoint spans; sorted merge and re-encode otherwise.
    pub fn merge(&self, other: &Self) -> Self;
    pub fn to_bytes(&self) -> Vec<u8>;
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, FormatError>;
}

// asap-executor: summary_kernels/compressed_samples.rs, operators/decode_samples.rs
pub(crate) enum StateStorage { /* ... */ CompressedSamples(SampleBlocks) }
```

Deployment JSON, unchanged shape: `{"excluded_candidates": [{"summary_family":
"compressed_samples"}]}` or `[{"operator": "DecodeSamplesInRange"}]`.

### Design scope

| New item | Scope | Why it is general |
| --- | --- | --- |
| `FieldDataType::CompressedSamples` | IR schema: a state type | A lossless window state of any numeric series, not tied to one query |
| `DecodeSamplesInRange` | IR operator, Stage 1 Pass 2 candidate, executor operator | The bridge from stored windows to raw rows, which every exact operator reads |
| `add_compressed_sample_chains` | Stage 1 Pass 2 rule | Alternative producers for raw-range groups, as `add_chain_groups` is for state groups |
| `column_compressed_bits_per_sample` | data facts | A per-column data property, like `column_magnitudes` |
| `CompressedSamples` kernel | `asap_sketchlib` | A portable state format beside the sketches' |

## 6. Decisions

| Decision | Evidence | Rejected, and why |
| --- | --- | --- |
| A window state in a chain, not an encoding of raw rows kept across evaluations | Windows give sealing, retention, persistence and tiering per window, and chain sharing, which exist for chains; raw rows kept across evaluations are excluded whenever raw data is not retained, which is the case to fix | Compressing the `Scan` kept across evaluations: one store per `Scan` group (three for `data` in typical-dashboard), excluded by the flag |
| Decode to raw rows and reuse the exact operators | Every function in section 2.1 is already answered exactly from raw rows; one new operator instead of one readout per function | Per-function readouts on the state (`SummaryEstimate` of a quantile, a max, …): duplicates the exact operators' semantics (counter resets, extrapolation, staleness) |
| Read overlapping windows and the open window, trim both edges | Grid-aligned windows for `data` in typical-dashboard (readers every 10, 15, 20, 30 s) must be 5 s wide: 5 samples per window and 1.01 GB kept, against 0.30 GB at 5 min | Aligned windows only: 3.4× the memory, or one chain per reader grid |
| Key the chain by series identity only; derive labels at decode | Three `data` scans differ only in projected labels; one chain instead of three | A key per projected label: 16 bytes more per label per window and no sharing |
| No compressed-samples levels in covers | Decode cost is per sample and the bytes are the samples; a wider level saves only 80 bytes per window | Wider levels by concatenation |
| Kernel in `asap_sketchlib` | It hosts the portable formats and golden vectors the data plane reads; executor kernels wrap it | Executor-only codec: a second implementation for any other data plane |
| Assumed 72 bits/sample when undeclared | Random doubles measure 58 bits, jitter adds 8; under-charging breaks memory limits | Assuming a production average (11 bits): Stage 3 would pick it on data that takes 5× more |
| Keep `raw_data_retained` | It states a deployment's raw store and stored data; validation depends on it | Replacing it: the plan's chain cannot stand in for a store of historical data |

**Open questions:**

1. **Open-window read in the runtime contract.** The decode reads the chain's
   open window. That is a planner-runtime contract addition for this state's
   chains only. Alternatively, a deployment without it could be offered
   grid-aligned widths (and pay the memory above). Recommended: the open-window
   read, with `{"operator": "DecodeSamplesInRange"}` excluded where missing.
2. **Measuring bits per sample.** Who declares `column_compressed_bits_per_sample`
   for a workload: the workload author, or a calibration that encodes a sample
   of the data? Recommended: a calibration step that writes the fact, like the
   measured operation costs.
3. **Mixed covers** (sketch interior, decoded edges): follow-up, or never?

## 7. Test plan (failing tests first)

Each test is written before the code and fails on main for the reason given.

| # | Test (crate) | Checks | Fails on main because |
| --- | --- | --- | --- |
| K1 | `round_trip_is_bitwise` (asap_sketchlib) | Regular and irregular timestamps (every delta-of-delta bucket, a 1 h gap), values incl. NaN payloads, ±0, ±∞, subnormals, equal runs, random doubles: decode equals input bit for bit | No codec |
| K2 | `portable_format_golden_vectors` (asap_sketchlib) | Fixed inputs encode to fixed bytes; the bytes decode back | As K1 |
| K3 | `merge_disjoint_concatenates_overlapping_resorts` (asap_sketchlib) | Disjoint spans: bytes of the merge equal the concatenated encode; overlapping: equals the encode of the sorted union | As K1 |
| K4 | `samples_in_trims_half_open` (asap_sketchlib) | `(a, b]` at, before and after sample times; empty block; one sample | As K1 |
| K5 | `encoded_bits_match_the_cost_formula` (asap_sketchlib + asap-cost) | Regular 1 s timestamps cost 1 bit each; for the K1 streams, `state_bytes` equals `to_bytes().len()` | No formula |
| T1 | `compressed_samples_family_json` (asap-types) | `SummaryFamily::of(&CompressedSamples)`; `"compressed_samples"` round-trips; an exclusion with it validates | No variant |
| T2 | `decode_schema_equals_time_range_schema` (asap-types) | For a PromQL scan projecting `job`, and one projecting `region, service`: `DecodeSamplesInRange` over the identity-keyed chain derives the `TimeRange ← Scan` group's schema | No operator |
| P1 | `raw_range_groups_get_decode_candidates` (stage1-pass2) | typical-dashboard, raw data not retained: every `TimeRange ← Scan` group gains one decode candidate per width; exactly 2 metrics × 3 widths chain groups; the three `data` scans share them | No rule |
| P2 | `decode_edges_ignore_the_evaluation_grid` (stage1-pass2) | A 5 min chain serves a reader every 10 s with range 5 min and offset 7 s | As P1 |
| P3 | `no_decode_for_non_series_sources` (stage1-pass2) | A SQL table without a (key, time) unique key, and a string value column: no candidate | Passes on main; guards the rule |
| P4 | `no_compressed_levels_in_covers` (stage1-pass2) | No `SummaryMergeWindows` or `TimeRangeCover` over a compressed chain | Passes on main; guards section 3.2 |
| C1 | `compressed_window_bytes` (asap-cost) | 300 samples at 58.2 bits: `24 + 2,183 = 2,207` state bytes, 2,287 per window row; with nothing declared, 72 bits and a `CompressedBitsPerSample` assumption in the report | No state type |
| C2 | `decode_charged_per_sample_in_range` (asap-cost) | 1 h at 1 sample/s, w = 5 min, 10,000 series: `(3,600 + 150) × 10,000` samples × decode ns per evaluation; a range request charges `(W + span + w/2)` once | No operator |
| C3 | `chain_keeps_longest_range_plus_one_window` (asap-cost) | Readers of 5 min and 1 h: the chain keeps `ceil(3,600 / 300) + 1 = 13` windows per series | As C1 |
| S1 | `memory_limit_selects_compressed_samples` (integration-tests) | typical-dashboard at 10,000 series with the measured bits declared and a 0.7 GB limit: a plan is found (main: none fits); both quantiles read through decode; predicted kept bytes within 5% of section 4 (0.59 GB) | No candidate |
| S2 | `selection_matches_exhaustive_oracle` (integration-tests) | `quantile_over_time(0.99, x[1h])` + `max_over_time(x[30m])`, 1,000 series, three memory limits: the MILP's choice equals the cheapest feasible of all {compressed, DDSketch, KLL, Max chain} assignments, each priced by `plan_cost` | As S1 |
| S3 | `exact_target_without_raw_data` (integration-tests) | `AccuracyTarget::exact()` on `quantile_over_time` with raw data not retained: a plan exists | Main: no plan |
| S4 | `excluding_the_family_restores_main` (integration-tests) | With `{"summary_family": "compressed_samples"}` excluded, the plans and costs of S1's workload equal main's | Passes on main; guards the exclusion |
| E1 | `decoded_rows_equal_raw_rows` (executor) | Range and instant reads, `T` on and off window boundaries, offsets, windows of 1 and 5 min: rows equal `TimeRange ← Scan` bit for bit | No operator |
| E2 | `exact_answers_equal_raw_plan` (executor) | `quantile_over_time`, `max/min/sum/count/avg_over_time`, `rate` and `increase` with a counter reset, `last_over_time`, `topk` over values: answers equal the raw-row plan's bit for bit, every 10 s for 30 min | As E1 |
| E3 | `open_window_is_read` (executor) | A query at `T` mid-window sees the samples of `(window start, T]` | As E1 |
| E4 | `out_of_order_within_window_is_exact` (executor) | Shuffled arrival within a window: the sealed block equals the in-order encode | No kernel |
| E5 | `sealed_bytes_match_cost_model` (executor) | The sealed column's bytes per state equal `compressed_samples_state_bytes` + 8 | As E4 |
| E6 | `decode_holds_one_series_per_thread` (executor) | Peak held rows during a 1 h read of 1,000 series ≤ one series' rows × query threads | As E1 |
| E7 | `sealed_window_bytes_are_the_portable_format` (executor) | A sealed state's persisted bytes equal `CompressedSamples::to_bytes` | As E4 |
| I1 | `installable_plan_round_trips_compressed_samples` (planner) | An `InstallablePlan` with the chain and the decode round-trips JSON and compiles on the executor | No operator |
| M1 | calibration (devtools) | Measured encode and decode ns per sample enter `measured_operation_costs`; section 3.3's 25 and 10 ns are replaced | Expected diff, reviewed |
| D1 | plan snapshots (devtools) | Changed snapshots listed with old → new cost and memory | Expected diff, reviewed |

E1 and E2 compare against the raw-row plan on the same data, so they need no
expected values computed by the planner.
