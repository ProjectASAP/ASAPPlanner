# Example: Key code interfaces section

An excerpt from ASAPPlanner #715 (HLL sized from (ε, δ)) in the expected layout:
links per file group at a fixed commit, real declarations with every member
listed and marked, a `///` on every interface, and an end-to-end usage example
whose every variable is commented and whose numbers come from running it.

## Key code interfaces added / modified

[`crates/types/src/post_asap/sketch.rs`](https://github.com/ProjectASAP/ASAPPlanner/blob/1b5d9577705c7664e711eef9a83ba4f2def0c1da/crates/types/src/post_asap/sketch.rs)

```rust
/// Concrete parameters of one sketch instance; equality compares the built shape.
/// The type stays; this PR adds a field to `Hll` and `Kmv`.
pub enum SketchParams {
    /// A UnivMon universal sketch (frequency moments, entropy, distinct count).
    UnivMon {                                        // unchanged
        /// Top-k heap entries per layer.
        heap_size: u32,                              // unchanged
        /// Rows of each layer's Count Sketch.
        sketch_rows: u32,                            // unchanged
        /// Columns of each layer's Count Sketch.
        sketch_cols: u32,                            // unchanged
        /// Number of sampling layers.
        layers: u8,                                  // unchanged
        /// The δ the heap was sized for when it answers a distinct count; None = no distinct guarantee.
        distinct_failure_probability: Option<f64>,   // unchanged
    },
    /// A KLL quantile sketch.
    Kll {                                            // unchanged
        /// Compactor capacity.
        k: u32,                                      // unchanged
        /// The δ `k` was sized for when below 0.01; None = the empirical 99% calibration.
        failure_probability: Option<f64>,            // unchanged
    },
    /// A Count-Min sketch.
    Cms {                                            // unchanged
        /// Counters per row.
        width: u32,                                  // unchanged
        /// Rows.
        depth: u32,                                  // unchanged
    },
    /// A HyperLogLog distinct-count sketch.
    Hll {                                            // unchanged variant; gains relative_error
        /// `m = 2^precision` registers.
        precision: u8,                               // unchanged
        /// The relative error `precision` was sized for: its guarantee's error bound,
        /// at which its failure probability is evaluated.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        relative_error: Option<f64>,   // new: the ε precision was sized for; None = RSE 1.04/√m; ignored by equality
    },
    /// A DDSketch relative-error quantile sketch.
    DDSketch {                                       // unchanged
        /// Relative accuracy of each bucket.
        alpha: f64,                                  // unchanged
    },
    /// A Count-Min sketch with a top-k heap.
    CmsWithHeap {                                    // unchanged
        /// Counters per row.
        width: u32,                                  // unchanged
        /// Rows.
        depth: u32,                                  // unchanged
        /// Heap entries.
        heap_size: u32,                              // unchanged
    },
    /// A KMV (k minimum values) distinct-count sketch.
    Kmv {                                            // unchanged variant; gains relative_error
        /// The number of smallest hashes kept.
        k: u32,                                      // unchanged
        /// The relative error `k` was sized for: its guarantee's error bound, at which
        /// its failure probability is evaluated.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        relative_error: Option<f64>,   // new: the ε k was sized for; None = RSE 1/√(k-2); ignored by equality
    },
    /// A Theta distinct-count sketch.
    Theta {                                          // unchanged
        /// Nominal entries.
        k: u32,                                      // unchanged
    },
    /// A Count Sketch.
    CountSketch {                                    // unchanged
        /// Counters per row.
        width: u32,                                  // unchanged
        /// Rows.
        depth: u32,                                  // unchanged
    },
    /// A Count Sketch with a top-k heap.
    CountSketchWithHeap {                            // unchanged
        /// Counters per row.
        width: u32,                                  // unchanged
        /// Rows.
        depth: u32,                                  // unchanged
        /// Heap entries.
        heap_size: u32,                              // unchanged
    },
}
```

[`crates/asap-aware-mapping/src/accuracy/estimators/hll.rs`](https://github.com/ProjectASAP/ASAPPlanner/blob/1b5d9577705c7664e711eef9a83ba4f2def0c1da/crates/asap-aware-mapping/src/accuracy/estimators/hll.rs)

```rust
/// Pass 1's classic HLL precision for relative error `eps` at failure probability
/// `delta`, without data; plan selection later enlarges it for the source's bound.
pub(crate) fn hll_precision(eps: f64, delta: f64) -> u8;   // was hll_precision(eps)
/// Classic HLL's distinct-count guarantee over `source`'s own rows, with δ left
/// symbolic in the source's distinct bound; `None` for a precision outside the contract.
pub(crate) fn classic_guarantee(
    precision: u8,
    relative_error: Option<f64>,   // new parameter
    source: &str,
) -> Option<AccuracyGuarantee>;
/// The failure probability given the source's distinct bound; 1 (no certificate)
/// outside the contract's range. Plan selection calls it to bind the symbol.
pub(crate) fn classic_failure_probability(
    precision: u8,
    relative_error: f64,   // new parameter
    max_distinct: u64,
) -> f64;
```

Usage, end to end: one SQL distinct count with ε = 0.05 and δ = 0.01, planned
through `run_planning_stages` at the merge commit. It starts from what the user
writes, names the stage that calls each interface above with its values, and
ends at what the user gets back.

```rust
// 1. What the user writes: one dashboard panel, the number of distinct source IPs
//    in the last minute, refreshed every 10 s, within ε = 0.05 relative error
//    with failure probability δ = 0.01.
let panel = RepeatingEntry {
    query: Query("SELECT COUNT(DISTINCT src_ip) FROM flows WHERE ts >= now() - INTERVAL '1 minute'".into()),
    demand: RepeatedDemand::FixedInterval(RepetitionInterval(10_000)),
    requirements: QueryRequirements {
        // The target every candidate plan's guarantee is checked against in Stage 3.
        accuracy: AccuracyRequirement::Explicit(RequestedAccuracy {
            scope: AccuracyScope::WholeResult,
            ..RequestedAccuracy::epsilon_delta(0.05, 0.01)
        }),
        response_latency: LatencyRequirement::Unspecified,
    },
    predictability: Predictability::Predictable { known_at: None },
    time_selection: TimeSelection {
        scope: QueryTimeScope::RealTime,
        lookback: Some(DurationMs(60_000)),
        as_of: None,
    },
};
// 2. What the deployment declares about the data: at most 1,000 distinct items in
//    one evaluation of any distinct count over `flows` (the SQL table name is the
//    source key). Stage 1 never reads it; Stage 3 uses it to turn HLL's symbolic δ
//    into a number.
let data = DataWorkload {
    arrival: DataArrival::ContinuouslyIngesting,
    ingestion_rate: declared(Rate(1_000_000.0 / 15.0)),
    max_distinct_per_evaluation: declared(BTreeMap::from([("flows".into(), 1_000)])),
    ..Default::default()
};
// The planner input: the query workload plus the data facts.
let workload = PlanningWorkload {
    query_workload: QueryWorkload {
        language: QueryLanguage::SQL(SqlDialect::DataFusionSQL),
        query_batch: None,
        repeating_queries: Some(vec![panel]),
    },
    data_workload: Some(data),
};
// The `flows(ts, src_ip)` schema the SQL frontend resolves the query against.
let catalog = asap_frontend_sql::SqlCatalog::new().with_table(
    "flows",
    Schema::with_time_index(
        vec![
            Field::plain("ts", DataType::Timestamp, false),
            Field::plain("src_ip", DataType::Utf8, false),
        ],
        0,
        vec![],
    ),
);

// 3. Stage 0 (query-language frontends): `roots` holds one plan root per workload entry,
//    here a distinct count (`aggregate:cardinality`) over a scan of `flows`; Stage 1 reads it.
let roots = lower_sql_roots(&workload, &catalog)?;

// 4. Stages 1-3 in one call. `run` holds every stage's candidates and the selection,
//    which steps 5 and 6 read.
let run = run_planning_stages(&workload, roots, None).expect("Pass 1 enumerates");
//    Stage 1 / Pass 1 offers L1 exact, L2 HLL, L3 KMV and L4 UnivMon for the count.
//    For L2 it calls `size_params(Hll, &intent, 0.05, 0.01)`: the smallest m with
//    m ≥ 1/(2ε²δ) whose tail certifies δ at ε for every bound up to ⌈1/ε⌉ = 20,
//    i.e. `Hll { precision: 15, relative_error: Some(0.05) }`.
//    It then calls `hll::classic_guarantee(15, Some(0.05), "flows")`: error bound
//    0.05 (relative), and δ left as the symbol
//    `classic_hll_failure_probability[p=15,eps=0.05](max_distinct[flows])`,
//    because the distinct count it depends on is a data fact Stage 1 does not read.
//    L3 needs no bound: `Kmv { k: 4323 }` certifies δ = 0.009996 for any count.
//
// 5. Stage 3 (plan selection) binds the symbol: it parses (15, 0.05, "flows"), reads
//    the bound 1,000 from `max_distinct_per_evaluation`, and calls
//    `hll::classic_failure_probability(15, 0.05, 1_000)` = 0.0058 ≤ 0.01, so P2 (HLL)
//    meets the target without enlargement (`precision_for` keeps 15; precision 15
//    certifies bounds up to 1,176, precision 18 up to 9,636).
// `selection`: Stage 3's result (selected id, costs, rejections); Err only when no candidate is valid.
let selection = run.stage3_plan_selection?;

// 6. What the user gets back.
//    `selected`: the cheapest valid plan. The analytical cost model prices the
//    exact plan P1 at 1.7600001 cost/s and HLL (P2) and KMV (P3) at 1.7600003 (the
//    same scan and build, plus an estimate node), so exact is selected; HLL is valid
//    but costlier.
assert_eq!(selection.selected, "P1");
//    `costs`: one entry per valid candidate (P1-P4; UnivMon P4 costs 2.96).
assert_eq!(selection.costs.len(), 4);
//    `rejected`: P2's line is `valid: true, reason: "costlier: 1.760 vs 1.760
//    cost_per_second"`. With a bound of 10,000 instead, P2 is `valid: false`,
//    "failure probability Some(1.0), which misses target ..." (beyond 9,636 even at
//    precision 18); with no bound, "failure probability None".
// `hll`: the HLL candidate's line in the selection report, read to show why it was not chosen.
let hll = selection.rejected.iter().find(|r| r.id == "P2").unwrap();
assert!(hll.valid && hll.reason.starts_with("costlier"));
```
