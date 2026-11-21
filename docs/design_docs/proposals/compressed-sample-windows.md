# Compressed sample windows

**Status:** Accepted (design and test plan); tests written, implementation pending.

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
one series in one window, in timestamp order, encoded as one **block**. A
block names its **codec** in its first byte; the codec is chosen per block when
the window seals (section 2.2), so it is not a planner parameter: the state
has no θ, and a chain group of it has one build. A new codec is a new tag and
a new encoder and decoder in the kernel; nothing in the planner changes but the
calibrated bits per sample (section 3.3).

Two codecs, fixed here to the bit so that every implementation writes the same
bytes (`asap_sketchlib` keeps golden vectors for them). Integers are
little-endian; "varint" is a zigzag LEB128 varint of a signed integer.

**Codec 0, Gorilla (XOR).** The encoding of the Gorilla paper (VLDB 2015).
Header, 25 bytes: codec `0` (`u8`); sample count `u32`; first timestamp `i64`
(ms); first value as `f64` bits `u64`; payload length in bits `u32`. Payload:
for each sample after the first, its timestamp code then its value code, bits
written most significant first, the last byte padded with zeros.

| Part | Code |
| --- | --- |
| Timestamp | `d = t − t_prev` (the first delta is taken against `d_prev = 0`), `dod = d − d_prev`: `0` → `0`; [−64, 63] → `10` + 7 bits; [−256, 255] → `110` + 9 bits; [−2048, 2047] → `1110` + 12 bits; otherwise `1111` + 64 bits; two's complement |
| Value | `x = bits(v) XOR bits(v_prev)`: `x = 0` → `0`. Otherwise `lead = min(leading_zeros(x), 31)`, `trail = trailing_zeros(x)`; if a previous window `(L, T)` exists with `lead ≥ L` and `trail ≥ T` → `10` + the `64 − L − T` bits `x >> T`; else `11` + `lead` (5 bits) + `width − 1` (6 bits, `width = 64 − lead − trail`) + the `width` bits `x >> trail`, and `(lead, trail)` becomes the window |

It is lossless on any `f64` bit pattern: NaN payloads (PromQL stale markers),
±0 and ±∞ come back bit for bit. An empty block is 25 zero bytes.

**Codec 1, decimal + zstd.** For values that are short decimals, as most
exported metrics are (a latency in ms with two decimals, an integer counter).
A block is eligible when every value `v` is finite, not −0, and equals
`m / 10^D` exactly (`f64` division of the integer `m` by `10^D`) for one
`D ≤ 17` and integers `|m| < 2^53`; `D` is the largest of the values' smallest
such digit counts. Header, 19 bytes: codec `1`; sample count `u32`; first
timestamp `i64`; `D` (`u8`); mantissa order `u8` (`1`: deltas, `2`: deltas of
deltas); frame length `u32`. Then one zstd frame (the encoder uses level 1; a
decoder accepts any level) whose content is: for each sample after the first,
the varint of its timestamp's delta of delta (as codec 0 computes it); then for
every sample the varint of its mantissa's delta (order 1, from `m_prev = 0`) or
delta of delta (order 2, from previous delta 0). The encoder writes the order
whose varints are shorter (order 1 on a tie). Values decode as
`m as f64 / 10^D`, exactly the encoded values.

**Choosing the codec.** Sealing encodes the block with codec 0 and, when the
block is eligible, codec 1, and keeps the shorter (codec 0 on a tie). The
window still open is always codec 0, appended sample by sample (section 2.2).

A sample whose timestamp equals one already in the block replaces its value
(the last write wins).

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

**Merge.** Two blocks of one series merge into the block that sealing the
union of their samples gives (on equal timestamps the second block's value
wins): exact, and byte-identical to building the union directly. Disjoint
codec-0 blocks can be joined without decoding the first; that is an
optimization, not a different result. No plan in this design merges across
windows (section 3.2); the merge is the kernel's `merge_with` for
shard-parallel builds and for persistence compaction.

### 2.2 Build and seal at ingestion

The chain `SummaryTumblingWindowChain { summary_type: CompressedSamples,
input: value, reduction: PerEntity, tumbling_window_width_ms: w }` over a scan
of the metric builds one block per series per window, as every chain builds its
states. The input row is `(series identity, timestamp, value)`; the window time
index is the chain's.

- **Build:** each sample is appended to its series' open block in codec 0, bit
  by bit, so the open window stays compact and readable (section 2.3). A sample
  older than the newest one in the open block (out of order within the window)
  re-encodes the open block with the sample inserted, so the block stays exact;
  its cost is charged only when the data workload declares out-of-order
  arrival.
- **Seal:** at the window's end (plus the deployment's allowed lateness, as for
  every chain) the block is re-encoded with the codec that is shorter
  (section 2.1) and shrunk to its encoded length. Sealing pays the codec-1
  encode once per sample (33–103 ns, section 3.3).

### 2.3 Read at query time: `DecodeSamplesInRange`

A new ASAP operator reads a compressed-samples chain and returns the raw sample
rows of one range, `(T − offset − range, T − offset]`, per series:

1. Select the chain's windows that **overlap** the range.
2. Decode each window's block in time order.
3. Keep only the samples inside the range (for an instant read: the newest
   sample at or before `T − offset` within the lookback).
4. Derive the label columns the reader needs from each series' identity, and
   emit rows series-major, in bounded batches.

An offset is a `TimeShift` between the decode and the chain
(`DecodeSamplesInRange { child: TimeShift(offset) ← chain }`), as Pass 2
re-wraps the shifts it strips for every chain. Its output has exactly the
schema and rows of `TimeRange(range) ← TimeShift* ← Scan` over the same
metric. It is a candidate of that group, so every exact consumer of raw
rows (the `Aggregate` of `quantile_over_time`, `max_over_time`, `rate`, …)
reads it unchanged.

**Two read modes**, one field of the operator, `reads_unsealed_window`:

- **`true`: sealed windows and the unsealed one.** Step 1 includes the window
  still open at `T` (its samples so far), so the chain's width need not divide
  the readers' evaluation grid, range or offset: one chain per metric serves
  every reader. This is a runtime contract (section 3.6): the runtime lets a
  query read the open window of a compressed-samples chain. A deployment
  states it with the capability `unsealed_window_reads` (default `true`).
- **`false`: sealed windows only.** The chain's width divides the reader's
  evaluation grid, range and offsets (the widths `add_chain_groups` uses), so
  the windows ending in `(T − W, T]` tile the range and nothing is trimmed. A
  deployment with `unsealed_window_reads: false` gets only these.

### 2.4 Kernel home: `asap_sketchlib`

The encoder, decoder, merge and portable byte format live in `asap_sketchlib`
as `CompressedSamples`, next to the sketches whose portable formats and golden
vectors it already hosts. The executor wraps it as it wraps every sketch
(`summary_kernels/`). It is not a `SketchAlgorithm` (it has no error and no θ),
so it is a separate type, not a variant of that enum. `asap_sketchlib` has no
sample codec today (no Gorilla, XOR or delta-of-delta code under `src/`). Codec
1 adds a `zstd` dependency to `asap_sketchlib`.

### 2.5 Planner vocabulary

- `FieldDataType::CompressedSamples` and `SummaryFamily::CompressedSamples`
  (JSON `"compressed_samples"`), so a deployment can exclude it:
  `{"summary_family": "compressed_samples"}`.
- `ASAPOp::DecodeSamplesInRange` and `OperatorKind::DecodeSamplesInRange`, so a
  deployment whose runtime lacks the operator excludes
  `{"operator": "DecodeSamplesInRange"}`.
- The capability `DeploymentCapabilities::unsealed_window_reads` (default
  `true`). `false` expands into the exclusion `{"operator":
  "DecodeSamplesInRange", "reads_unsealed_window": true}`
  (`ExclusionSource::UnsealedWindowsNotReadable`), leaving the aligned
  widths.
- A data fact per source column, `column_compressed_bits_per_sample`, which
  the calibration step measures and writes (section 3.3), and the assumption
  recorded when it is missing, `DataFactAssumption::CompressedBitsPerSample`
  (cost only).

## 3. Algorithm

### 3.1 Pass 2: one compressed-samples chain per metric

New Pass 2 rule `add_compressed_sample_chains`, run after `add_chain_groups`:

```
for each group R = TimeRange(W, kind) ← TimeShift*(Scan(source, columns)):
    if source is not a series source with one numeric value column: skip
    X  = the memo group of Scan(source, [time, value, series identity])
    for w in ladder widths(source):               # unsealed reads
        C_w = group SummaryTumblingWindowChain[CompressedSamples, w](X)
        add to R: DecodeSamplesInRange { child: shifts(C_w), range: W,
            kind, labels, reads_unsealed_window: true }
    for w in aligned widths(R):                   # sealed windows only
        C_w = the same chain group at width w
        add to R: DecodeSamplesInRange { child: shifts(C_w), range: W,
            kind, labels, reads_unsealed_window: false }
```

`labels` are the columns R carries besides time, value and identity;
`shifts(C_w)` re-wraps R's `TimeShift`s around the chain group.

- **Series sources.** A PromQL metric (series identity, time, value), or a SQL
  table whose unique key is (series key columns, time index) read for one
  numeric column. Other tables get no candidate in this design.
- **Keyed by identity only.** The chain is built over a scan of time, value and
  series identity, so the scans that differ only in the labels they project
  (typical-dashboard has three `Scan` groups of `data`, projecting `job`,
  `region`, and `region, service`) share one chain. The labels are a function
  of the identity; the decode derives them.
- **Ladder widths (unsealed reads).** The ladder of
  [multi-level-window-chains.md](multi-level-window-chains.md) — 1 min, 5 min,
  15 min — limited to widths no longer than the metric's longest reader range.
  The grid does not constrain them (section 2.3). Stage 3 picks one by cost:
  a wider window holds fewer per-window bytes, but its readers decode on
  average half a window of samples outside their range, and the chain keeps one
  more window. Their edges are exempt from
  `ChainWidthDoesNotDivideEvaluationGrid`, and Stage 3's delivery check treats
  the decode's read as reaching the open window.
- **Aligned widths (sealed reads).** The gcd closure of `gcd(g_q, W, o_1, …)`
  over R's readers, as `add_chain_groups` computes it; the grid rule applies to
  their edges as to any chain's. They exist so that a deployment without
  unsealed-window reads still has the state.

Example (typical-dashboard): groups `TimeRange 300s/900s/1800s/3600s ← Scan`
over `data` (five groups) and over `data_total` (four) each gain three
unsealed-read candidates; the ladder adds two metrics × three widths = six
chain groups, and the aligned widths (5, 10, 15, 20, 30 s for `data`) add the
others.

**Complexity.** At most 3 ladder widths per metric plus the aligned widths
`add_chain_groups` already enumerates, and one decode candidate per raw-range
group and width. The MILP gains those nodes and edges; no new constraint
family.

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
  (overlap, then trim; for a sealed read, the windows ending in `(T − W, T]`),
  so no `TimeRange` node reads this state.
- **Follow-up, not built: mixed covers.** A cover whose interior is wide
  sketch windows and whose edges are decoded samples (folded into the sketch)
  would cut the narrow sketch level's memory. It needs a cover over levels of
  different state types; it is listed here as a follow-up and is not part of
  this design or its tests.

### 3.3 Cost model

Per window state (`summary_state_bytes`, from `StateInputRows::rows_per_state`
= samples per series per window `n`, and `b` = the column's calibrated bits per
sample):

```
state_bytes(n, b)  = 25 + ceil(n · b / 8)                # codec-0 header + payload
window_row_bytes   = state_bytes + 8 (blob offset) + state_row_bytes_beside_state
                   = state_bytes + 8 + 72               # identity 64, window end 8
```

A codec-1 block's header is 6 bytes shorter; the formula charges the codec-0
header for both, at most 6 bytes over.

**Where `b` comes from.** The calibration step measures it: it seals blocks of
a sample of each source column's data with the kernel, as the executor would
(`asap_cost::compressed_bits_per_sample`: payload bits, after the header, over
samples), and writes the result into the data facts as
`column_compressed_bits_per_sample[column]`, beside the measured operation
costs it already writes. Because it measures the sealed blocks, it measures
whichever codec wins on that data. When the fact is absent, the cost model uses
`ASSUMED_COMPRESSED_BITS_PER_SAMPLE = 72` (9 bytes) and records
`DataFactAssumption::CompressedBitsPerSample`. 72 bits covers i.i.d. random
doubles (measured 59 bits) plus jittered timestamps (9 bits).

**Codec bake-off.** Reference implementations of codec 0, and of codec 1 with
zstd level 1, with lz4 instead of zstd, and with no general-purpose compression
(varints only), 2,000 series, one sample per second at a regular interval,
blocks of 300, 600 and 3,600 samples (5 min at 1 Hz, 1 min at 10 Hz, 1 h at
1 Hz), round trip checked bit for bit, one core of a 2.0 GHz x86 server.
Bits per sample include each block's header. Encode and decode in ns per
sample.

| Data shape | Block | Codec 0 (XOR): bits · enc · dec | Codec 1, zstd: bits · enc · dec | Decimal + lz4: bits · enc · dec | Decimal, varints only: bits |
| --- | --- | --- | --- | --- | --- |
| current generator: Pareto(2) gauge, i.i.d. doubles | 300 / 600 / 3,600 | 58.9 · 31 · 12.9 / 58.8 · 31 · 12.5 / 59.0 · 27 · 12.1 | not eligible (not short decimals) | not eligible | not eligible |
| current generator: counter `(n + 1) · scale`, non-integer | 300 / 600 / 3,600 | 58.2 / 58.0 / 58.2 | not eligible | not eligible | not eligible |
| realistic: log-normal latency, 2 decimals | 300 | 58.7 · 25 · 13.0 | **19.3** · 103 · 17.3 | 19.1 · 63 · 16.0 | 26.7 |
| | 600 | 58.5 · 31 · 12.6 | **18.5** · 81 · 18.7 | 18.6 · 59 · 15.3 | 26.4 |
| | 3,600 | 58.6 · 29 · 14.8 | **17.4** · 64 · 18.4 | 18.2 · 54 · 15.0 | 26.1 |
| realistic: integer counter, +1…19 per sample | 300 | 14.9 · 26 · 12.6 | **6.6** · 62 · 22.4 | 9.2 · 34 · 10.8 | 16.8 |
| | 600 | 14.9 · 25 · 11.6 | **5.4** · 47 · 16.4 | 8.6 · 31 · 10.2 | 16.4 |
| | 3,600 | 16.4 · 23 · 11.0 | **4.3** · 33 · 11.8 | 8.1 · 29 · 10.0 | 16.1 |
| realistic: smooth gauge, 1 decimal | 300 | 15.8 · 13 · 8.7 | **3.6** · 54 · 15.4 | 4.2 · 44 · 12.5 | 16.8 |
| | 600 | 15.3 · 13 · 8.4 | **2.6** · 54 · 17.6 | 3.6 · 41 · 12.0 | 16.4 |
| | 3,600 | 15.4 · 13 · 7.9 | **1.5** · 42 · 14.1 | 3.1 · 37 · 11.4 | 16.1 |

What it shows:

- **XOR is the wrong codec for decimal values.** A latency rounded to two
  decimals has a random binary mantissa, so XOR spends 59 bits on it, as on a
  random double. Decimal mantissas with deltas take 26 bits; zstd over them,
  17–19. A time-series database that stores decimal mantissas, deltas and zstd
  blocks reports about 10 bits per sample on the benchmark's new
  latency-and-counter data, against 60 (gauge) and 11.6 (counter) for XOR.
- **zstd over lz4.** On latencies they tie (19.3 vs 19.1 bits); on counters
  and smooth gauges zstd is 1.4–2× smaller (6.6 vs 9.2, 3.6 vs 4.2 at 300
  samples). lz4 decodes 1–6 ns per sample faster. Memory is the binding limit
  this design addresses, so codec 1 uses zstd.
- **Keep XOR as codec 0.** Values that are not short decimals (random doubles,
  computed ratios, NaN stale markers) are not eligible for codec 1, and the
  open window needs a codec that appends one sample at a time; XOR does both.
- **Costs.** Codec 1 decodes in 12–22 ns per sample against 8–15 for XOR, and
  encodes once per sample at seal in 33–103 ns. Both are below a DDSketch
  chain's 340 ns per row at ingestion.

The analytical constants until a deployment calibrates them:
`ANALYTICAL_ENCODE_NS_PER_COMPRESSED_SAMPLE = 100` (append plus the seal's
re-encode) and `ANALYTICAL_DECODE_NS_PER_COMPRESSED_SAMPLE = 20`.

The other terms:

| Term | Formula | Charged on |
| --- | --- | --- |
| Kept memory | `window_row_bytes × (ceil(W_max / w) + 1) × series`, `W_max` the longest reader range | the chain, `AcrossEvaluations(Memory)`, as every chain |
| Ingestion CPU | `encode_ns × samples/s` (100 ns analytical until calibrated; DDSketch's chain build is 340 ns per row, measured) | the chain |
| Decode CPU per evaluation | `decode_ns × (W + w/2) · rate × series` for unsealed reads, `decode_ns × W · rate × series` for sealed reads (20 ns analytical until calibrated) | `DecodeSamplesInRange` |
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
  data[1h])` at 10 samples/s decodes 363 million samples per evaluation, 7.3
  core-seconds at 20 ns. The 10 s KLL chain merges 3.6 million states, 12.4 s
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
| Seal | re-encode the block with the shorter codec (section 2.1), shrink to its encoded length | chain seal |
| Merge | concatenate disjoint blocks; sorted merge and re-encode for overlapping ones (`AggregateCore::merge_with`) | |
| Read | `DecodeSamplesInRange`: request the windows overlapping the range, plus, with `reads_unsealed_window`, the open window's snapshot; decode, trim, derive labels, emit series-major batches | the materialized-view read, widened by one window |
| Evaluate | none new: the decoded rows go to the raw-row operators (`range_function`, `aggregate`, …) | all exact evaluation |

**Runtime contract: unsealed-window reads.** A runtime that declares
`unsealed_window_reads` (the Planner's executor does) answers a
`DecodeSamplesInRange { reads_unsealed_window: true }` read with the chain's
sealed windows in the range and a snapshot of its open window: the builder
publishes each open block's (sample count, payload bits) after each appended
batch, and the reader copies the bytes up to them. The snapshot holds every
sample that arrived before the read with a timestamp in the open window, as a
raw scan would. The contract covers compressed-samples chains only; no other
chain is read before it seals.

Range functions over series-major input free each series' rows after
evaluating it, so query memory is bounded by one series per thread.

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
- **Format version.** A block's first byte names its codec; the fragment's
  format version covers the rest, and `asap_sketchlib` keeps golden vectors
  for each codec.

Placing chains in a file tier is a Stage 2 choice (`StorageMedium::Disk`), to
be designed with that deployment capability; this design keeps the chain in
memory and prices it there.

## 4. Predicted memory at 10,000 series

From the section 3.3 formulas, with the least-memory plan's other chains
(section 1) where they are smaller, on both data shapes: the current benchmark
generator (i.i.d. random values: codec 0 everywhere, codec 1 is not eligible)
and the realistic generator the benchmark is switching to (log-normal
latencies with two decimals, integer counters: codec 1 wins every block). Bits
per sample are the bake-off's at the block size the window holds; a smooth
gauge is shown as a third `data` shape.

**typical-dashboard** (1 sample/s, longest range 1 h for both metrics; today
3.17 GB of `data` chains + 0.49 GB of `data_total` chains = 3.66 GB, 3.72 GB
peak; limit 0.5 GB). Kept bytes `data` + `data_total` at the best width,
5 min (300-sample blocks); add about 0.06 GB of query-time memory (today's
peak over average):

| Data shape (bits/sample `data` / `data_total`) | Kept | Fits 0.5 GB |
| --- | --- | --- |
| current generator, codec 0: 58.9 / 58.2 | 0.30 + 0.29 = **0.59 GB** | no (6.2× below today) |
| realistic, codec 1: latency 19.3 / integer counter 6.6 | 0.11 + 0.05 = **0.15 GB** | yes, about 0.21 GB with query-time memory |
| realistic, codec 1: smooth gauge 3.6 / integer counter 6.6 | 0.03 + 0.05 = 0.08 GB | yes |
| realistic data with codec 0 only (XOR): 58.7 / 14.9 | 0.30 + 0.08 = 0.38 GB | just (0.44 GB) |
| nothing declared (72 / 72 assumed) | 0.36 + 0.36 = 0.73 GB | no |

**dashboard-heavy** (10 samples/s, longest range 1 h; today `data` 4.25 GB
(KLL + Max) and `data_total` 0.78 GB (counters), 5.10 GB peak; limit 3.7 GB;
query-time memory about 0.07 GB):

| Data shape | `data` compressed | `data_total` | Total kept | Fits 3.7 GB |
| --- | --- | --- | --- | --- |
| current generator, codec 0: 58.8 / 58.0, w = 1 min | 2.73 GB | counters kept, 0.78 GB (compressed: 2.68 GB) | **3.51 GB**, about 3.58 GB peak | yes, by 0.12 GB |
| realistic, codec 1: 18.5 / 5.4, w = 1 min (600-sample blocks) | 0.91 GB | compressed, 0.31 GB | **1.22 GB** | yes |
| realistic, codec 1: 17.4 / 4.3, w = 5 min (3,000-sample blocks, the 3,600-sample measurement) | 0.86 GB | compressed, 0.22 GB | 1.08 GB | yes |
| realistic data with codec 0 only: 58.5 / 14.9, w = 1 min | 2.74 GB | compressed, 0.81 GB → counters 0.78 GB | 3.52 GB | just |
| nothing declared (72 / 72) | 3.36 GB | counters, 0.78 GB | 4.13 GB | no |

So on the realistic data codec 1 cuts the compressed chains about 3× against
XOR, and both workloads fit with room; the calibrated fact matters, since the
assumed default fits neither.

Query-time CPU (decode only, at the analytical 20 ns/sample):
typical-dashboard 0.17 core-seconds per second over its 11 readers (each 1 h
reader 0.75 s per evaluation, versus 2.03 s for today's 1 h DDSketch merge);
dashboard-heavy 1.1 core-seconds per second (the 1 h quantile 7.3 s per
evaluation, versus 12.4 s for today's KLL merge). Ingestion: 100 ns per sample,
2 ms/s for typical-dashboard and 20 ms/s for dashboard-heavy.

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
    /// compressed-samples chain `child` that overlap it; with
    /// `reads_unsealed_window`, the open window too. Same schema and rows as
    /// `TimeRange(range) ← Scan`; `labels` are derived from each series'
    /// identity.
    DecodeSamplesInRange {
        child: C,
        range: Duration,
        kind: TimeRangeKind,
        labels: Vec<String>,
        reads_unsealed_window: bool,
    },
}

// asap-types: ir/operator/kind.rs
pub enum OperatorKind { /* ... */ DecodeSamplesInRange }

// asap-types: stages/logical (origin of a candidate)
pub enum OriginOfCandidateForMemoGroup { /* ... */ CompressedSampleWindows }

// asap-types: deployment_capabilities/mod.rs
pub struct DeploymentCapabilities {
    /// Whether the runtime answers a read of a compressed-samples chain with
    /// its open window too; default `true`. `false` leaves sealed reads at
    /// aligned widths.
    pub unsealed_window_reads: bool,
}
pub enum ExclusionSource { /* ... */ UnsealedWindowsNotReadable }

// asap-types: deployment_capabilities/exclusion.rs
pub struct CandidateExclusion {
    /// Matches a `DecodeSamplesInRange` by its read mode.
    pub reads_unsealed_window: Option<bool>,
}

// asap-types: workload/data.rs
pub struct SourceFacts {
    /// Bits one sample of each column takes compressed, keyed by column
    /// (default: none declared), e.g. `{"value": 58.2}`; written by the
    /// calibration step.
    pub column_compressed_bits_per_sample: BTreeMap<String, f64>,
}

// asap-types: workload/assumed_data_fact.rs
pub enum DataFactAssumption {
    /// No `column_compressed_bits_per_sample` for `column`: its compressed
    /// samples are costed at `assumed_bits_per_sample`. Cost only.
    CompressedBitsPerSample { column: String, assumed_bits_per_sample: f64 },
}

// asap-cost
pub const ASSUMED_COMPRESSED_BITS_PER_SAMPLE: f64 = 72.0;
pub const ANALYTICAL_ENCODE_NS_PER_COMPRESSED_SAMPLE: f64 = 100.0;
pub const ANALYTICAL_DECODE_NS_PER_COMPRESSED_SAMPLE: f64 = 20.0;
/// 25 + ceil(samples · bits_per_sample / 8).
pub fn compressed_samples_state_bytes(samples: u64, bits_per_sample: f64) -> u64;
/// Calibration: payload bits over samples of `blocks`, each sealed by the kernel.
pub fn compressed_bits_per_sample(blocks: &[Vec<(i64, f64)>]) -> f64;
pub enum StateFamily<'a> { /* ... */ CompressedSamples }

// asap-stage1-pass2-reuse-candidates: rules/compressed_samples.rs
pub(crate) fn add_compressed_sample_chains(memo: &mut Memo, /* ... */);

// asap_sketchlib: compressed_samples.rs (re-exported at the crate root)
/// A block's codec, its first byte.
pub enum SampleCodec { Gorilla = 0, DecimalZstd = 1 }
pub struct CompressedSamples { /* codec + header + payload */ }
pub enum CompressedSamplesFormatError { Truncated, PayloadLengthMismatch, PayloadWithoutSamples, UnknownCodec, InvalidFrame }
impl SampleCodec {
    /// 25 for `Gorilla`, 19 for `DecimalZstd`.
    pub const fn header_bytes(self) -> usize;
}
impl CompressedSamples {
    pub fn new() -> Self;
    /// Appends a sample to the open (codec 0) block; an older one re-encodes
    /// the block with it inserted; an equal timestamp replaces the value.
    pub fn push(&mut self, timestamp_ms: i64, value: f64);
    /// The block re-encoded with the shorter codec (codec 0 on a tie).
    pub fn seal(&self) -> Self;
    /// The block re-encoded with `codec`; `None` when the samples are not
    /// eligible for it (codec 1: not all short decimals).
    pub fn encoded_with(&self, codec: SampleCodec) -> Option<Self>;
    pub fn codec(&self) -> SampleCodec;
    pub fn len(&self) -> u32;
    pub fn is_empty(&self) -> bool;
    /// Bits after the header: exact for codec 0, 8 · (bytes − 19) for codec 1.
    pub fn payload_bits(&self) -> u64;
    /// `codec().header_bytes() + ceil(payload_bits / 8)` = `to_bytes().len()`.
    pub fn encoded_bytes(&self) -> usize;
    pub fn iter(&self) -> impl Iterator<Item = (i64, f64)> + '_;
    /// Samples with `start_exclusive_ms < t <= end_inclusive_ms`, in time order.
    pub fn samples_in(&self, start_exclusive_ms: i64, end_inclusive_ms: i64) -> impl Iterator<Item = (i64, f64)> + '_;
    /// The sealed block of both blocks' samples (the second wins on equal timestamps).
    pub fn merge(&self, other: &Self) -> Self;
    pub fn to_bytes(&self) -> Vec<u8>;
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, CompressedSamplesFormatError>;
}

// asap-executor: summary_kernels/compressed_samples.rs, operators/decode_samples.rs
/// The executor kernel around the codec, as `DDSketchAccumulator` wraps its sketch.
pub struct CompressedSamplesAccumulator { /* CompressedSamples */ }
impl CompressedSamplesAccumulator {
    pub fn sketch(&self) -> &CompressedSamples;
    pub fn restore_from_disk(samples: CompressedSamples) -> Result<Self, KernelError>;
}
pub(crate) enum StateStorage { /* ... */ CompressedSamples(SampleBlocks) }
// asap-executor: sources/memory.rs (the in-memory store tests and runtimes use)
impl InMemoryDataStore {
    /// The open window's snapshot `(start_ms, t_ms]` of a compressed-samples
    /// view, replacing the previous one; what an unsealed read receives.
    pub fn set_open_window(&mut self, view: &MaterializedView, window_ms: (i64, i64), batches: Vec<Batch>);
}
```

Deployment JSON, unchanged shape: `{"unsealed_window_reads": false}`,
`{"excluded_candidates": [{"summary_family": "compressed_samples"}]}` or
`[{"operator": "DecodeSamplesInRange"}]`.

### Design scope

| New item | Scope | Why it is general |
| --- | --- | --- |
| `FieldDataType::CompressedSamples` | IR schema: a state type | A lossless window state of any numeric series, not tied to one query |
| `DecodeSamplesInRange` | IR operator, Stage 1 Pass 2 candidate, executor operator | The bridge from stored windows to raw rows, which every exact operator reads |
| `add_compressed_sample_chains` | Stage 1 Pass 2 rule | Alternative producers for raw-range groups, as `add_chain_groups` is for state groups |
| `unsealed_window_reads` | deployment capabilities | A runtime property, expanded into an exclusion like the others |
| `column_compressed_bits_per_sample` | data facts | A per-column data property, like `column_magnitudes` |
| `CompressedSamples` kernel | `asap_sketchlib` | A portable state format beside the sketches' |

## 6. Decisions

Approved with the design: the open-window read as a runtime contract with a
capability; bits per sample measured by calibration; mixed covers a follow-up.

| Decision | Evidence | Rejected, and why |
| --- | --- | --- |
| A window state in a chain, not an encoding of raw rows kept across evaluations | Windows give sealing, retention, persistence and tiering per window, and chain sharing, which exist for chains; raw rows kept across evaluations are excluded whenever raw data is not retained, which is the case to fix | Compressing the `Scan` kept across evaluations: one store per `Scan` group (three for `data` in typical-dashboard), excluded by the flag |
| Decode to raw rows and reuse the exact operators | Every function in section 2.1 is already answered exactly from raw rows; one new operator instead of one readout per function | Per-function readouts on the state: duplicates the exact operators' semantics (counter resets, extrapolation, staleness) |
| Read overlapping windows and the open window, trim both edges, as a runtime contract with the capability `unsealed_window_reads` | Grid-aligned windows for `data` in typical-dashboard (readers every 10, 15, 20, 30 s) must be 5 s wide: 1.01 GB kept, against 0.30 GB at 5 min | Aligned windows only: 3.4× the memory. They stay as the candidates for a deployment without the capability |
| Key the chain by series identity only; derive labels at decode | Three `data` scans differ only in projected labels; one chain instead of three | A key per projected label: 16 bytes more per label per window and no sharing |
| No compressed-samples levels in covers; mixed covers a follow-up | Decode cost is per sample and the bytes are the samples; a wider level saves only 80 bytes per window | Wider levels by concatenation |
| Kernel in `asap_sketchlib` | It hosts the portable formats and golden vectors the data plane reads; executor kernels wrap it | Executor-only codec: a second implementation for any other data plane |
| Pluggable codec per block, chosen at seal: XOR (codec 0) and decimal mantissas + deltas + zstd (codec 1) | Bake-off, section 3.3: on latencies with two decimals XOR takes 58.7 bits per sample, codec 1 19.3; on integer counters 14.9 vs 6.6; on random doubles codec 1 is not eligible and XOR takes 58.9. A time-series database with the decimal-mantissa scheme stores the benchmark's new data at about 10 bits per sample | XOR only: 3× the memory on realistic data. Decimal only: cannot store random doubles or NaN stale markers, and cannot append to the open window. lz4 instead of zstd: ties on latencies, 1.4–2× larger on counters and smooth gauges |
| Bits per sample measured by calibration into the data facts; 72 bits only when absent | Shapes differ 5× (14 vs 66 bits, section 3.3); under-charging breaks memory limits | A production average (11 bits) as default: Stage 3 would pick it on data that takes 6× more |
| Keep `raw_data_retained` | It states a deployment's raw store and stored data; validation depends on it | Replacing it: the plan's chain cannot stand in for a store of historical data |

**Follow-ups (not in this design or its tests):** mixed covers (sketch
interior, decoded edges); a decimal-scaled integer encoding for few-decimal
values; file-tier placement of chains (`StorageMedium::Disk`) with the data
plane's tiered storage capability.

## 7. Test plan (failing tests first)

Thirty tests, written before the code. Each fails on main for the reason given
(most because the interface does not exist yet, so the test target does not
build). The codec tests K1–K5 live in `asap_sketchlib`; ASAPPlanner pins that
commit. The executor's test target is `cargo test -p asap-executor --test executor`.

| # | Test (crate) | Checks | Fails on main because |
| --- | --- | --- | --- |
| K1 | `round_trip_is_bitwise` (asap_sketchlib) | Regular and irregular timestamps (every delta-of-delta bucket, a 30-day gap); values incl. NaN payloads, ±0, ±∞, subnormals, equal runs, random doubles (sealed as codec 0), and two-decimal latencies and integer counters (sealed as codec 1); shuffled pushes equal sorted pushes; an equal timestamp keeps the last value: `iter()` of the open block, of the sealed block and of `from_bytes(to_bytes())` equals the input bit for bit | No codec |
| K2 | `portable_format_golden_vectors` (asap_sketchlib) | Codec 0: fixed inputs encode to fixed bytes, computed by an independent reference implementation of section 2.1. Codec 1: a fixed block's 19 header bytes and its frame's decompressed content (the varint stream) are fixed, computed the same way; a frame written at another zstd level decodes to the same samples. Malformed bytes (unknown codec, truncated header, payload length mismatch, samples without payload, invalid frame) are refused with the right error | As K1 |
| K3 | `merge_equals_sealed_union` (asap_sketchlib) | The merge's bytes equal the sealed block of the union, for disjoint and interleaved spans, a codec-0 block with a codec-1 block, and an empty block; the second block's value wins on equal timestamps | As K1 |
| K4 | `samples_in_trims_half_open` (asap_sketchlib) | `(a, b]` at, before and after sample times, for the open block and both sealed codecs; empty block; one sample | As K1 |
| K5 | `codec_choice_and_code_lengths` (asap_sketchlib) | Codec 0 of n constant samples 1 s apart takes 16 + 1 + 2 (n − 2) payload bits; header bytes are 25 and 19; sealing keeps the shorter codec: two-decimal latencies and integer counters → codec 1, random doubles → codec 0, a decimal block with one NaN → codec 0 (`encoded_with(DecimalZstd)` is `None`); `encoded_bytes` = `to_bytes().len()` | As K1 |
| T1 | `compressed_samples_family_json` (asap-types) | `SummaryFamily::of(&CompressedSamples)`; `"compressed_samples"` round-trips; an exclusion with it validates | No variant |
| T2 | `decode_schema_equals_time_range_schema` (asap-types) | For a PromQL scan projecting `job`, and one projecting `region, service`: `DecodeSamplesInRange` over the identity-keyed chain has the `TimeRange ← Scan` group's schema | No operator |
| T3 | `unsealed_window_reads_false_excludes_unsealed_decodes` (asap-types) | Default capabilities expand into no such exclusion; `unsealed_window_reads: false` expands into `{operator: DecodeSamplesInRange, reads_unsealed_window: true}` from `UnsealedWindowsNotReadable`, and it matches only the unsealed read mode | No capability |
| P1 | `raw_range_groups_get_decode_candidates` (stage1-pass2) | A typical-dashboard-shaped workload, raw data not retained: every `TimeRange ← Scan` group gains one unsealed-read decode per ladder width; two metrics × three widths ladder chain groups; the three `data` scans share them | No rule |
| P2 | `unsealed_decode_edges_ignore_the_evaluation_grid` (stage1-pass2) | A 5 min chain serves a reader every 10 s with range 5 min and offset 7 s | As P1 |
| P3 | `no_decode_for_non_series_sources` (stage1-pass2) | A SQL table without a (key, time) unique key: no decode candidate | No rule (the test names the new operator) |
| P4 | `compressed_chains_are_read_only_by_decodes` (stage1-pass2) | Every candidate reading a compressed-samples chain group is a `DecodeSamplesInRange`: no merge, cover or wider level over it | As P3 |
| P5 | `sealed_decodes_use_aligned_widths` (stage1-pass2) | The sealed-read decodes' chain widths are the gcd closure of each group's readers; their edges fail `ChainWidthDoesNotDivideEvaluationGrid` exactly where a chain's would | As P1 |
| C1 | `compressed_window_bytes` (asap-cost) | 300 samples at 58.2 bits: 25 + 2,183 = 2,208 state bytes; the formula equals the kernel's `encoded_bytes` for codec-0 blocks whose bits per sample are exact, and is at most 6 bytes over for codec-1 blocks; a chain node's window row is 2,288 bytes, 297.44 MB kept for 1 h of 10,000 series | No formula |
| C2 | `decode_charged_per_sample_in_range` (asap-cost) | 1 h at 1 sample/s, w = 5 min, 10,000 series: `(3,600 + 150) × 10,000` samples per evaluation for an unsealed read, `3,600 × 10,000` for a sealed one | No operator |
| C3 | `chain_keeps_longest_range_plus_one_window` (asap-cost) | Retained for its longest reader (1 h), the 5 min chain keeps `ceil(3,600 / 300) + 1 = 13` windows per series; with nothing declared, 72 bits (25 + 2,700 = 2,725 state bytes) and a `CompressedBitsPerSample` assumption naming the source | As C1 |
| M1 | `calibrated_bits_are_the_encoders` (asap-cost) | `compressed_bits_per_sample` over two sealed 256-sample blocks that are not short decimals ((i + 1) / 3 and the constant 1/3) is 16,787 + 525 payload bits over 512 samples = 33.8125, the reference implementation's count; over a two-decimal block it is the sealed codec-1 block's payload bits over its samples; a workload declaring the value is costed with it (25 + 1,268 bytes per 300-sample window) and records no assumption | No calibration function |
| S1 | `memory_limit_selects_compressed_samples` (integration-tests) | typical-dashboard-shaped workload at 10,000 series with calibrated bits declared and a 0.7 GB limit: a plan is found (main: none fits); both quantiles read through decode; predicted kept bytes within 5% of section 4 | No candidate |
| S2 | `selection_matches_exhaustive_oracle` (integration-tests) | `quantile_over_time(0.99, x[1h])` + `max_over_time(x[30m])`, 1,000 series, three memory limits: the selected plan's cost equals the cheapest feasible one among the enumerated assignments {compressed, DDSketch, KLL, Max chain}, each priced by `plan_cost` | As S1 |
| S3 | `exact_target_without_raw_data` (integration-tests) | `AccuracyTarget::exact()` on `quantile_over_time` with raw data not retained: a plan exists and reads through decode | Main: no plan |
| S4 | `excluding_the_family_restores_main` (integration-tests) | With `{"summary_family": "compressed_samples"}` excluded, no plan node is compressed; with `unsealed_window_reads: false`, every decode reads sealed windows only | No exclusion vocabulary |
| E1 | `decoded_rows_equal_raw_rows` (executor) | Range and instant reads, `T` on and off window boundaries, offsets, windows of 1 and 5 min: rows equal `TimeRange ← Scan` bit for bit | No operator |
| E2 | `exact_answers_equal_raw_plan` (executor) | `quantile_over_time`, `max/min/sum/count/avg_over_time`, `rate` and `increase` with a counter reset, `last_over_time`, `topk` over values: answers equal the raw-row plan's bit for bit, every 10 s for 30 min | As E1 |
| E3 | `open_window_is_read` (executor) | A query at `T` mid-window sees the samples of `(window start, T]` with an unsealed read, and not with a sealed one | As E1 |
| E4 | `out_of_order_within_window_is_exact` (executor) | Shuffled arrival within a window: the sealed block equals the in-order encode | No kernel |
| E5 | `sealed_bytes_match_cost_model` (executor) | The sealed column's bytes per state equal `compressed_samples_state_bytes` + 8 for the block's measured bits | As E4 |
| E6 | `decode_holds_one_series_per_thread` (executor) | Peak held rows during a 1 h read of 1,000 series ≤ one series' rows × query threads | As E1 |
| E7 | `sealed_window_bytes_are_the_portable_format` (executor) | A sealed state's persisted bytes equal `CompressedSamples::to_bytes` | As E4 |
| I1 | `installable_plan_round_trips_compressed_samples` (planner) | An `InstallablePlan` with the chain and both decode modes round-trips JSON and compiles on the executor | No operator |
| D1 | `compressed_samples_plan_snapshot` (devtools) | The typical-dashboard-shaped plan text under a memory limit matches a reviewed snapshot that reads both quantiles through decode | No candidate; snapshot written with the implementation |

E1 and E2 compare against the raw-row plan on the same data, and K2's bytes
come from a separate reference implementation, so no expected value is computed
by the code under test.
