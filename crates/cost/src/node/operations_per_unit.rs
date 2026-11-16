//! The CPU operations the formulas charge per unit of work: per row a time
//! range keeps, per row a hash aggregate groups, per row folded into a
//! summary state, per row and label column a grouping carries, per group
//! and raw batch a grouping visits, per state merged and per byte of a KLL
//! state merged or read, per output group of a merge, per group read out
//! and per output row built.
//!
//! One CPU operation is one nanosecond of the reference executor's work on
//! one core: each count below is the executor's measured time for that
//! unit, in ns, and [`ResourceSpeed::ms_per_cpu_op`](crate::ResourceSpeed)
//! (1e-6 by default) turns operations into milliseconds on the deployment's
//! machine. What plan selection needs is their ratios: a sketch built at
//! query time is cheaper than the exact plan only when its update is
//! cheaper than keeping and sorting the same rows. These are the built-in
//! fallback; a table measured on the deployment
//! (`CostInputs::measured_operation_costs`) replaces them node by node.
//!
//! Operators not measured here still charge an abstract 1 operation per
//! unit, unscaled: the source scan (1 per sample plus 0.1 per byte), a
//! filter or projection (1 per row per predicate or expression), sorts,
//! joins, deduplication, set operations and the other non-summary
//! operators (`analytical`), and every node `usage` prices by its input
//! rows alone.
//!
//! Measured on one core of an idle server with the executor at commit
//! 59365ce80, in a release build: each query-time node of a plan run as the
//! only root over warm in-memory data, its own time the sub-plan's time
//! minus its inputs', median of 5 runs. Data: 1,000 series of a
//! non-negative gauge (`ts`, `value`, the series identity) and of a counter
//! (with a `job` label), 1 h windows at 1 Hz (3,600 rows per series,
//! tumbling windows of 10 s) and at 10 Hz (36,000 rows per series, windows
//! of 60 s). Each constant's own measurement is in its doc. On a host
//! with most cores busy, the sketch constants measure about twice as high
//! (DDSketch update 325–403 ns per row, merge 3.4–3.9 µs per state) while
//! the row-streaming ones (time range, hash aggregate, exact accumulators)
//! barely move: memory and cache contention slows sketch kernels most, which
//! only favours the exact plan further.
//!
//! Example: `quantile_over_time(0.99, latency[1h])` at 1 Hz reads 3.6 M
//! rows. The exact plan keeps them (`TIME_RANGE_OPERATIONS_PER_ROW`, 11),
//! groups them per series (`HASH_AGGREGATE_OPERATIONS_PER_ROW` +
//! `AGGREGATE_MEASURE_OPERATIONS_PER_ROW`, 20, plus
//! `OPERATIONS_PER_GROUP_PER_RAW_BATCH` once per series and 10 s batch, 17
//! per row) and sorts each series' 3,600 values (11.8 levels ×
//! `SORT_OPERATIONS_PER_ROW_PER_LEVEL`, 65): 113 ns per row. A DDSketch built
//! from the same rows takes 11 + `DDSKETCH_UPDATE_OPERATIONS_PER_ROW` (210) +
//! 17 = 238 ns per row, about twice the exact plan, as measured (111 and
//! 229).

// Each rule lists every variant, as in `usage`: a wildcard arm would give a
// new operator, summary type or sketch a default cost instead of failing
// the build (under `-D warnings`).
#![warn(
    clippy::wildcard_enum_match_arm,
    clippy::match_wildcard_for_single_variants
)]

use asap_types::ir::operator::AggIntent;
use asap_types::ir::schema::FieldDataType;
use asap_types::summary_type::{ExactKind, SketchParams};

use crate::node::state_bytes::StateInputRows;
use crate::StateFamily;

/// A row kept by a time range over raw rows, or a state read by a time
/// range over a chain's tumbling windows: 9–16 ns per row (median 11) over
/// 54 time ranges of 30,000 to 36 M rows at 1 Hz and 10 Hz over 100, 1,000
/// and 10,000 series, 8–18 ns per state over 360,000 states. A query-time
/// tumbling window chain pays it too, per raw row of its scan's lookback,
/// which it keeps in its window's range.
pub(crate) const TIME_RANGE_OPERATIONS_PER_ROW: f64 = 11.0;

/// Per group of a grouping (a hash aggregate, a summary build or a
/// tumbling window chain), per batch of raw rows its rows arrive in
/// ([`ResourceSpeed::raw_batch_width_ms`](crate::ResourceSpeed)): finding
/// the group's state again for the next batch. It is why the same grouping
/// costs more per row at a lower sample rate: a 10 s batch holds 10 rows
/// of a series at 1 Hz and 100 at 10 Hz, so this term adds 17 ns per row
/// at 1 Hz and 1.7 at 10 Hz.
///
/// Fitted together with the per-row constants below
/// (`HASH_AGGREGATE_OPERATIONS_PER_ROW` with one measure,
/// `EXACT_UPDATE_OPERATIONS_PER_ROW`, `COUNTER_UPDATE_OPERATIONS_PER_ROW`,
/// `DDSKETCH_UPDATE_OPERATIONS_PER_ROW`, `KLL_UPDATE_OPERATIONS_PER_ROW`
/// and `CHAIN_OPERATIONS_PER_ARRIVING_ROW`) over a grid of 245 workloads:
/// 1, 2, 5 and 10 Hz; 100, 1,000 and 10,000 series; 5 min, 30 min and 1 h
/// windows (at most 36 M rows); every 300 s, and every 60 s at 1,000
/// series over 5 min. The points are 572 query-time per-series hash
/// aggregates and summary builds (Max, Count, Sum and quantile aggregates;
/// Max, Sum, Count, Rate, Increase, DDSketch and KLL builds) and 254
/// stored plans' ingestion CPU. The loss and the terms were chosen by
/// leave-one-setting-out error (each of the 35 settings, one series count,
/// rate, window and interval together, left out in turn, its points
/// predicted by a fit on the rest): squared log error 0.147, absolute
/// 0.150. Leaving out each rate, each series count or each window instead
/// gives 0.148, 0.172 and 0.148, the squared loss again at or below the
/// absolute and the terms the same. A chain's term per group and
/// batch did not lower it (0.147 with it, fitted 15), so a chain pays only
/// this one. Fitted 171, then rounded with the others: measured ÷ charged
/// is 0.76–1.50 for per-series hash aggregates at 1 Hz and 0.87–1.26 at
/// 10 Hz, 0.66–1.48 and 0.93–1.30 for exact builds, 0.67–1.42 and
/// 0.81–1.16 for ingestion chains (RMS log error 0.143 over all points,
/// 0.155 with the constants fitted on 1 and 10 Hz alone). The error is
/// largest at 1 Hz and at 10,000 series, which nothing here follows.
pub(crate) const OPERATIONS_PER_GROUP_PER_RAW_BATCH: f64 = 170.0;

/// Per row an ingestion-time tumbling window chain receives: routing the
/// arriving row to its window's state, which a build at query time does
/// not do (fitted 13.8 with `OPERATIONS_PER_GROUP_PER_RAW_BATCH`). A
/// query-time chain instead keeps each row of its scan's lookback in its
/// window's range (`TIME_RANGE_OPERATIONS_PER_ROW`).
pub(crate) const CHAIN_OPERATIONS_PER_ARRIVING_ROW: f64 = 14.0;

/// Per input row of a hash aggregate grouped by one key or more: interning
/// the row's first key value in the key column's table of distinct values
/// and reading its group at that value's index (the executor's
/// `GroupTable` with one group column). With
/// `AGGREGATE_MEASURE_OPERATIONS_PER_ROW` and
/// `OPERATIONS_PER_GROUP_PER_RAW_BATCH`, a per-series `max_over_time` (one
/// key, one measure) groups a row in 16 + 4 + 17 ns at 1 Hz and 16 + 4 +
/// 1.7 at 10 Hz (fitted 20.4 for one key and one measure). A SQL
/// aggregate grouped by one key measures about 4 ns per row less (12.8 +
/// 3.8 when fitted on its own).
pub(crate) const HASH_AGGREGATE_OPERATIONS_PER_ROW: f64 = 16.0;

/// Per input row, for each grouping key beyond the first: interning that
/// key's value too and finding the group by the tuple of values in a hash
/// map, which a grouping by one key does not need. Fitted on SQL hash
/// aggregates over 1 to 3 keys and 1 to 4 measures, 100 to 10,000 groups
/// of 30 to 3,000 rows, 10 and 100 rows per group per raw batch (65
/// aggregates, minimizing the absolute log error, chosen by
/// leave-one-setting-out error 0.156 against 0.167 for the squared): 28 per
/// further key and 3.8 per measure, beside 12.8 for the first key. A
/// second key measures 29–34 ns per row more than one key, a third 16–20
/// more than two.
pub(crate) const HASH_AGGREGATE_OPERATIONS_PER_FURTHER_KEY: f64 = 28.0;

/// Per input row, for each plain measure of a hash aggregate (sum, count,
/// min, max, ...): folding the row's value into its group's. Three more
/// measures (`MIN`, `SUM`, `COUNT` beside `MAX`) measure 10–12 ns per row
/// more (see `HASH_AGGREGATE_OPERATIONS_PER_FURTHER_KEY`).
pub(crate) const AGGREGATE_MEASURE_OPERATIONS_PER_ROW: f64 = 4.0;

/// Per input row, per level of the sort that an exact quantile runs over
/// its group's values (`log2` of the rows per group levels): a per-series
/// exact quantile costs 72 ns per row more than a per-series max at 3,600
/// rows per series (11.8 levels, 6.1 ns per level) and 69–97 ns more at
/// 36,000 (15.1 levels, 4.5–6.4 ns per level).
pub(crate) const SORT_OPERATIONS_PER_ROW_PER_LEVEL: f64 = 5.5;

/// One row folded into a DDSketch state, beyond
/// `OPERATIONS_PER_GROUP_PER_RAW_BATCH` (fitted 210.8 on the grid of
/// `OPERATIONS_PER_GROUP_PER_RAW_BATCH`): a query-time build measures
/// 0.83–1.45x its charge at 1 Hz and 0.81–0.96x at 10 Hz.
pub(crate) const DDSKETCH_UPDATE_OPERATIONS_PER_ROW: f64 = 210.0;

/// One row folded into a KLL state, beyond
/// `OPERATIONS_PER_GROUP_PER_RAW_BATCH` (fitted 83.5 on the grid of
/// `OPERATIONS_PER_GROUP_PER_RAW_BATCH`): a query-time build measures
/// 0.68–1.55x its charge at 1 Hz and 0.97–1.23x at 10 Hz. A state of fewer
/// rows than its `k` is cheaper, as it compacts nothing.
pub(crate) const KLL_UPDATE_OPERATIONS_PER_ROW: f64 = 84.0;

/// One row folded into an exact accumulator that keeps one value (sum,
/// count, min, max), beyond any label it carries
/// (`CARRIED_LABEL_OPERATIONS_PER_ROW_PER_LABEL`) and
/// `OPERATIONS_PER_GROUP_PER_RAW_BATCH` (fitted 28.7 on the grid of
/// `OPERATIONS_PER_GROUP_PER_RAW_BATCH`): an exact Max, Sum or Count built
/// at query time measures 0.66–1.34x its charge at 1 Hz and 0.97–1.30x at
/// 10 Hz.
pub(crate) const EXACT_UPDATE_OPERATIONS_PER_ROW: f64 = 29.0;

/// One row folded into an exact accumulator of a counter's increase (rate,
/// increase, irate), which also tracks resets and the first and last
/// samples, beyond any label it carries and
/// `OPERATIONS_PER_GROUP_PER_RAW_BATCH`: built per series carrying a `job`
/// label at query time it takes 66–110 ns per row over 22 nodes at 1 Hz
/// and 10 Hz over 100, 1,000 and 10,000 series, of which the label is
/// `CARRIED_LABEL_OPERATIONS_PER_ROW_PER_LABEL` (37). The time per row
/// follows the sample rate, not the rows per series, as the group and
/// batch term charges it (fitted 33.4 with it on the grid of
/// `OPERATIONS_PER_GROUP_PER_RAW_BATCH`): a Rate or Increase built at
/// query time measures 0.87–1.48x its charge at 1 Hz and 0.93–1.11x at
/// 10 Hz.
pub(crate) const COUNTER_UPDATE_OPERATIONS_PER_ROW: f64 = 33.0;

/// Per input row, for each label column a per-series state or group
/// carries beside its key (the `job` of a per-series sum that a later
/// `sum by (job)` reads): copying the label into the group. A per-series
/// exact `Aggregate` carrying `job` takes 51–77 ns per row where one
/// carrying nothing takes 18–38 at the same settings, and an exact Count
/// accumulator carrying it 79 ns where a Max carrying nothing takes 42:
/// 37 ns per row, the median difference. Refit at 1 Hz and 10 Hz over 100
/// to 10,000 series, a per-series Count carrying `job` built at query time
/// measures 74 ns per row and a Max 40 (medians of 12 nodes each): 34.
pub(crate) const CARRIED_LABEL_OPERATIONS_PER_ROW_PER_LABEL: f64 = 37.0;

/// One row folded into a KMV state, beyond the retained hashes it compares
/// the row's hash with (`KMV_UPDATE_OPERATIONS_PER_COMPARED_HASH`): reading
/// the row's value, finding its group's state, hashing the value and
/// checking it against the state's largest retained hash.
///
/// Measured on the executor's KMV `SummaryAgg` by (service, endpoint),
/// each build's time less its input's, median of 3 runs on one pinned
/// core, 10 rows per group per batch: 10 to 10,000 groups, 1,200 to
/// 60,000 rows per group, 10,000 to 1,000,000 distinct values, at k =
/// 1,081, 4,323, 26,656 and 106,263 (132 builds). The two settings a busy
/// host disturbed were measured three and four times and their median
/// kept. Of the forms compared by the error leaving one setting (groups,
/// rows per group, distinct values) out, a constant per row and one per
/// compared hash scores 0.058 (RMS of the log error; largest 0.25), and
/// 0.057 leaving each k out. Adding a term per inserted hash (the heap's
/// push), or per inserted hash times log2 k, leaves it at 0.058; a term
/// for the states' retained hashes outgrowing the cache brings it to 0.049
/// but fits a machine-specific cache size, so it is left out. Fitted 108.8
/// per row and 0.620 per compared hash, charged 110 and 0.62: measured ÷
/// charged is 0.81–1.27 over the 132 builds, against 1.17–1.63 at the
/// earlier 65 and 0.50. Example: 400 states at k = 4,323 over 60,000 rows
/// each of 1,000,000 distinct values compare 976 hashes per row, 715 ns
/// charged, 712 measured (553 charged before).
const KMV_UPDATE_OPERATIONS_PER_ROW: f64 = 110.0;

/// Per retained hash a KMV update compares the row's hash with
/// ([`kmv_compared_hashes_per_row`]): `asap_sketchlib` checks a new hash
/// for a duplicate by walking every hash it retains, unless the state is
/// full and the hash is above its largest, so an update costs O(k) while
/// the state fills and for every hash that enters it. That walk is the
/// library's, not the algorithm's: a set membership check would take O(1)
/// per update; once the library has one, this constant and
/// `KMV_UPDATE_OPERATIONS_PER_ROW` are measured again. Fitted with that
/// constant (see its doc). Example: 4 states by `region` at k = 26,656 over
/// 75,000 rows each of 100,000 distinct values compare 13,820 hashes per
/// row, 8,678 ns charged, 8,357 measured; one state at k = 4,323 over all
/// 300,000 rows fills early and compares 314, 305 ns charged, 329
/// measured.
const KMV_UPDATE_OPERATIONS_PER_COMPARED_HASH: f64 = 0.62;

/// Per hash a KMV merge reads from an input state, beyond the retained
/// hashes it compares that hash with
/// (`KMV_MERGE_OPERATIONS_PER_COMPARED_HASH`): `asap_sketchlib` merges by
/// updating the group's state with each hash the input retains, without
/// reading a row or hashing a value.
///
/// Measured on the executor's `SummaryMerge` over stored KMV states, median
/// of 3 runs on one pinned core: states of 1,200, 12,000 and 60,000 rows
/// of 1,000,000 distinct values, merged 5 per group (a 5-minute range of
/// 1-minute windows) and 10, 50 and 200 per group (a rollup), at k =
/// 1,081, 4,323, 26,656 and 106,263 (45 merges). Of the forms compared by
/// the error leaving one setting (k, rows per state, states per group)
/// out, a constant per hash and one per compared hash, with the states
/// sharing hashes (see [`merge_operations_per_state`]), scores 0.050 (RMS
/// of the log error; largest 0.10); taking every hash as distinct scores
/// 0.104, and also passing each group's first state through unmerged
/// 0.120. Adding a constant per state, or per hash times log2 k, leaves it
/// at 0.046. Fitted 2.61 per hash and 0.428 per compared hash, charged 2.6
/// and 0.43: measured ÷ charged is 0.90–1.08 over the 45 merges, against
/// 0.21–1.10 at the earlier 65 and 0.5. On 24 held-out rollups of states
/// merged from 5 windows (10 and 100 per group, 6,000 to 300,000 rows each,
/// some of 20,000 users) it is 0.84–1.14 (RMS log error 0.091, against
/// 0.436 at 65 and 0.5).
const KMV_MERGE_OPERATIONS_PER_HASH: f64 = 2.6;

/// Per retained hash a KMV merge compares an inserted hash with, as a
/// build's update does (`KMV_UPDATE_OPERATIONS_PER_COMPARED_HASH`); the
/// library's linear walk makes a merge of `k`-hash states O(k²), where a
/// sorted merge would be O(k). Fitted with `KMV_MERGE_OPERATIONS_PER_HASH`
/// (see its doc).
const KMV_MERGE_OPERATIONS_PER_COMPARED_HASH: f64 = 0.43;

/// Per input row of an exact distinct count or frequency statistic, beyond
/// its lookups and inserts below, the measure's own
/// `AGGREGATE_MEASURE_OPERATIONS_PER_ROW`, its grouping keys
/// (`HASH_AGGREGATE_OPERATIONS_PER_ROW`,
/// `HASH_AGGREGATE_OPERATIONS_PER_FURTHER_KEY`), its group visits
/// (`OPERATIONS_PER_GROUP_PER_RAW_BATCH`) and each group's output row
/// (`OUTPUT_ROW_OPERATIONS_PER_GROUP`), which are fixed at their
/// constants: reading the row's value and looking it and its pair up while
/// both tables fit in the cache, at `EXACT_DISTINCT_FEWEST_VALUES`.
///
/// Measured on the executor's exact `COUNT(DISTINCT user_id)` (median of 3
/// runs on one pinned core) over 10,000 series, 10 rows per series per
/// batch: 1,200,000, 12,000,000 and 36,000,000 rows of 1,000 to 10,000,000
/// distinct values, ungrouped and in 10, 100, 1,000 and 10,000 groups (75
/// aggregates); with an earlier grid over 1,000 series: 300,000 and
/// 3,000,000 rows of 100 to 100,000,000 values in 1 to 1,000 groups (44).
/// The terms are of the values the planner passes (at most the rows read).
/// Of the forms tried, the one with the lowest error leaving out each
/// (distinct values, rows, series) setting in turn (0.122 RMS log error;
/// 0.116 leaving out each group count, 0.148 each distinct count, 0.128
/// each row count) is this constant, a term per doubling of the values,
/// a cache-miss term for each table, and one per inserted value and per
/// inserted pair. Fitted 59.2, 12.9, 207, 79.2, 356 and 13.3, minimizing
/// the squared log error: measured ÷ charged 0.76–1.32 (RMS log 0.109),
/// where a constant, a term per square root of the values and one per
/// inserted entry gave 0.61–1.58 (0.224). Fitted on one grid alone, the
/// form predicts the other within 0.65–1.40.
const EXACT_DISTINCT_OPERATIONS_PER_ROW: f64 = 59.0;

/// Per doubling of the distinct values an exact distinct count's value
/// table holds past `EXACT_DISTINCT_FEWEST_VALUES` (at most
/// `EXACT_DISTINCT_MOST_VALUES`): a larger table spreads its lookups over
/// more cache lines (fitted 12.9). Example: 100,000 values are 6.6
/// doublings past 1,000, 86 per row.
const EXACT_DISTINCT_OPERATIONS_PER_VALUE_DOUBLING: f64 = 13.0;

/// The fewest distinct values on the 10,000-series grid: fewer are charged as
/// 1,000 (the earlier grid measured 60–92 ns per row over 100 values and
/// 58–123 over 1,000).
const EXACT_DISTINCT_FEWEST_VALUES: f64 = 1_000.0;

/// The most distinct values on the 10,000-series grid: more are charged as
/// 10,000,000 (the earlier grid measured 943–1,155 ns per row over
/// 100,000,000 values and 918–1,114 over 10,000,000).
const EXACT_DISTINCT_MOST_VALUES: f64 = 10_000_000.0;

/// Per input row when every value lookup misses the cache, so charged
/// times the fraction that miss ([`cache_miss_fraction`] of the values over
/// `EXACT_DISTINCT_VALUES_IN_CACHE`): a lookup in a value table that has
/// outgrown the cache reads memory (fitted 207). Example: 1,000,000 values
/// miss 0.8 of their lookups, 166 per row.
const EXACT_DISTINCT_OPERATIONS_PER_ROW_AT_FULL_VALUE_MISS: f64 = 207.0;

/// The distinct values whose table still fits in the cache: the best by
/// leave-one-setting-out error of 100,000, 200,000, 300,000, 500,000 and
/// 1,000,000 (0.122 at 200,000, 0.123–0.128 from 100,000 to 500,000).
/// Machine-specific: it follows the cache size of the machine the grid
/// ran on, so it is a default until calibration measures it per machine.
const EXACT_DISTINCT_VALUES_IN_CACHE: f64 = 200_000.0;

/// Per input row when every (group, value) pair lookup misses the cache,
/// so charged times the fraction that miss ([`cache_miss_fraction`] of the
/// pairs over
/// `EXACT_DISTINCT_PAIRS_IN_CACHE`, fitted 79.2).
const EXACT_DISTINCT_OPERATIONS_PER_ROW_AT_FULL_PAIR_MISS: f64 = 79.0;

/// The (group, value) pairs whose set still fits in the cache: the best by
/// leave-one-setting-out error of 500,000, 1,000,000, 3,000,000 and
/// 10,000,000 (0.122 at 500,000, 0.127 at 1,000,000, 0.134 at 3,000,000).
/// A pair is two 4-byte numbers, so fewer bytes than the value table
/// holds per value. Machine-specific: it follows the cache size of the
/// machine the grid ran on, so it is a default until calibration measures
/// it per machine.
const EXACT_DISTINCT_PAIRS_IN_CACHE: f64 = 500_000.0;

/// Per distinct value an exact distinct count interns into its value
/// table, shared over its input rows: storing the value and growing the
/// table (fitted 356). Example: 300,000 rows of 259,000 distinct values
/// pay 307 per row for them.
const EXACT_DISTINCT_OPERATIONS_PER_INSERTED_VALUE: f64 = 356.0;

/// Per (group, value) pair an exact distinct count inserts into its set of
/// pairs, shared over its input rows (fitted 13.3).
const EXACT_DISTINCT_OPERATIONS_PER_INSERTED_PAIR: f64 = 13.0;

/// The fraction of random lookups into a table of `entries` that miss a
/// cache holding `entries_in_cache` of them: none while the table fits,
/// then the share of the table outside the cache. Example: 2,000,000
/// entries in a cache of 500,000 miss 0.75 of their lookups.
fn cache_miss_fraction(entries: f64, entries_in_cache: f64) -> f64 {
    (1.0 - entries_in_cache / entries.max(1.0)).max(0.0)
}

/// The expected distinct values among `draws` uniform draws from a column
/// of `values` values: `values · (1 − e^(−draws / values))`. Example:
/// 12,000 rows of 1,000,000 users hold 11,929 distinct users.
fn expected_distinct_values(draws: f64, values: f64) -> f64 {
    -values * (-draws / values).exp_m1()
}

/// The operations one input row costs an exact distinct count (or
/// frequency statistic) over `rows` rows in `group_count` groups, of a
/// column taking `distinct_values` values, beyond the measure's own, its
/// grouping keys, its group visits and its groups' output rows: the
/// executor interns each value in one table of every distinct value read
/// and keeps each group's (group, value) pairs in a set, so its time per
/// row grows with both tables, sharply once one outgrows the cache, and
/// with the values and pairs it inserts. The pairs are each group's
/// expected distinct values among its rows, drawn from `distinct_values`.
/// Example: 12,000,000 rows of 1,000,000 values in 100 groups (11.3 M
/// pairs) cost 59 + 13 × 10.0 + 207 × 0.8 + 79 × 0.96 + 356 × 1 M ÷ 12 M +
/// 13 × 11.3 M ÷ 12 M = 472 (measured 488 ns per row with the key and the
/// measure, charged 492).
pub(crate) fn exact_distinct_value_operations_per_row(
    distinct_values: u64,
    group_count: u64,
    rows: u64,
) -> f64 {
    let values = distinct_values.max(1) as f64;
    let rows = rows.max(1) as f64;
    let groups = group_count.max(1) as f64;
    let pairs = groups * expected_distinct_values(rows / groups, values);
    let table_values = values.clamp(EXACT_DISTINCT_FEWEST_VALUES, EXACT_DISTINCT_MOST_VALUES);
    EXACT_DISTINCT_OPERATIONS_PER_ROW
        + EXACT_DISTINCT_OPERATIONS_PER_VALUE_DOUBLING
            * (table_values / EXACT_DISTINCT_FEWEST_VALUES).log2()
        + EXACT_DISTINCT_OPERATIONS_PER_ROW_AT_FULL_VALUE_MISS
            * cache_miss_fraction(table_values, EXACT_DISTINCT_VALUES_IN_CACHE)
        + EXACT_DISTINCT_OPERATIONS_PER_ROW_AT_FULL_PAIR_MISS
            * cache_miss_fraction(pairs, EXACT_DISTINCT_PAIRS_IN_CACHE)
        + (EXACT_DISTINCT_OPERATIONS_PER_INSERTED_VALUE * values.min(rows)
            + EXACT_DISTINCT_OPERATIONS_PER_INSERTED_PAIR * pairs)
            / rows
}

/// One counter update of any other sketch, per counter its row updates
/// ([`counters_updated_per_row`]), above
/// `EXACT_UPDATE_OPERATIONS_PER_ROW`, the executor's cost of handing a row
/// to its group's state. Not measured on the executor: `asap_sketchlib`'s
/// own Count-Min Sketch of depth 3 with a top-k heap updates in about
/// 72 ns, 18 ns per counter.
pub(crate) const SKETCH_UPDATE_OPERATIONS_PER_COUNTER: f64 = 18.0;

/// One exact accumulator state merged into its group's (sum, count):
/// 128–265 ns per state over 360,000 states.
pub(crate) const EXACT_MERGE_OPERATIONS_PER_STATE: f64 = 230.0;

/// One exact Min or Max state merged into its group's: 113–153 ns per
/// state over 100, 1,000 and 10,000 groups (a fit of 125 per state and
/// 0.86 µs per group, the group's part in `MERGE_OPERATIONS_PER_OUTPUT_GROUP`),
/// where a Sum or Count state measures about 230.
const EXTREMUM_MERGE_OPERATIONS_PER_STATE: f64 = 125.0;

/// One counter-increase accumulator state merged (rate, increase, irate):
/// 346 ns per state over 360,000 states.
pub(crate) const COUNTER_MERGE_OPERATIONS_PER_STATE: f64 = 350.0;

/// One DDSketch state merged: 1.8 µs per state of 10 rows, 2.5 µs per
/// state of 600 rows.
pub(crate) const DDSKETCH_MERGE_OPERATIONS_PER_STATE: f64 = 2_500.0;

/// One KLL state merged, beyond its bytes: the executor merges each input
/// state into a copy of its group's state, so a merge costs per byte of
/// the input state (`KLL_MERGE_OPERATIONS_PER_INPUT_BYTE`) and per byte of
/// the merged state (`KLL_MERGE_OPERATIONS_PER_MERGED_BYTE`), which grows
/// towards its `k`. The bytes are those the planner charges for each state
/// (`sketch_state_bytes` of the rows it folds), so the fit is over the
/// quantity it is applied to. Fitted over `asap_sketchlib`'s KLL at k = 200,
/// 547 and 1,404, states of 10 to 3,000 rows, 1 to 360 states per group
/// (1,000 groups, median of 3 runs), minimizing the squared log error:
/// within 2x of 20 of the 24 points, 0.41–2.4x. Example: merging 360
/// states of 10 rows (872 B each) per group, charged 17.0 KB merged at
/// k = 547 and 33.5 KB at k = 1,404 (the library holds 8.9 and 33.5 KB),
/// is charged 7.7 and 14.9 µs per state, measured 4.3 and 13.0 (4.7 and
/// 13.3 in the executor): the charge past `k` is the item array's
/// capacity, so the smaller `k` is over-charged and the two differ by
/// 1.9x where they measure 3x.
pub(crate) const KLL_MERGE_OPERATIONS_PER_STATE: f64 = 360.0;

/// Per byte of a KLL input state merged (see
/// `KLL_MERGE_OPERATIONS_PER_STATE`).
pub(crate) const KLL_MERGE_OPERATIONS_PER_INPUT_BYTE: f64 = 0.1;

/// Per byte of the merged KLL state each input state is merged into (see
/// `KLL_MERGE_OPERATIONS_PER_STATE`).
pub(crate) const KLL_MERGE_OPERATIONS_PER_MERGED_BYTE: f64 = 0.43;

/// Per output group of a `SummaryMerge`, beyond its input states' merges:
/// creating the group's state and its output row. Merging exact
/// accumulators over 1,000 groups costs 1.17 µs per group with one state
/// per group and 215 ns per state plus about 0.95 µs per group with 5 to
/// 360 (median of 20–26 runs per point). The time per group grows with the
/// groups: one-state merges on the fitting grid of
/// `OPERATIONS_PER_GROUP_PER_RAW_BATCH` measure a median 0.98–1.15 µs per
/// group at 100 and 1,000 groups (1.34 and 1.15 µs for exact
/// accumulators) and 1.5–1.8 µs at 10,000 (up to 2.5 µs for KLL states).
/// 1,100 minimizes neither alone but lowers the RMS log error of measured
/// ÷ charged over every merge on that grid from 0.281 at 950 to 0.248
/// (one state per group 0.375 → 0.273, several 0.214 → 0.233), and on the
/// held-out settings from 0.225 to 0.217. At 10,000 groups one-state
/// merges still measure a median 1.38–1.62x their charge (up to 2.29x for
/// KLL), at 1,000 groups 0.96x. Less than a finalize's or an estimate's
/// output row (`OUTPUT_ROW_OPERATIONS_PER_GROUP`, 2,500): a merge's row
/// takes its group's key columns and merged state as they are, where a
/// finalize or an estimate also reads each state into a new value column.
const MERGE_OPERATIONS_PER_OUTPUT_GROUP: f64 = 1_100.0;

/// Per byte of any other sketch state merged, above
/// `EXACT_MERGE_OPERATIONS_PER_STATE`: merging adds every counter of the
/// input state. Not measured on the executor: `asap_sketchlib` merges a
/// Count-Min Sketch of 16 KB in about 6 µs and a HyperLogLog of 16 KB in
/// about 13 µs, 0.4–0.8 ns per byte.
pub(crate) const SKETCH_MERGE_OPERATIONS_PER_BYTE: f64 = 1.0;

/// Per byte of the merged KLL state one group's answer is read from,
/// before its output row: the quantile sorts the items the state holds,
/// whose levels a merge leaves sorted. The bytes are those the planner
/// charges for the state (`sketch_state_bytes`); 0.9 per byte fits
/// `asap_sketchlib`'s KLL at k = 200 to 1,404 over the same 24 merged
/// states as `KLL_MERGE_OPERATIONS_PER_STATE` (median of 3 runs over 1,000
/// groups) within 2x at 21 of them. Example: a merged state of 360 states
/// of 10 rows is charged 15 µs at k = 547 (17.0 KB) and 30 µs at k = 1,404
/// (33.5 KB), measured 23 and 42.
pub(crate) const KLL_ESTIMATE_OPERATIONS_PER_BYTE: f64 = 0.9;

/// Per unsorted item of a KLL state built by updates (by a `SummaryAgg`,
/// or a chain's window read without a merge) that one group's answer is
/// read from, before its output row: the quantile sorts the newest items,
/// which updates leave unsorted, and there are at most `k` of them (the
/// state's rows while it holds fewer). The time is nearly flat in the
/// state's rows, unlike its bytes. Fitted on the executor at k = 547 over
/// 100, 1,000 and 10,000 series of states of 300 to 36,000 rows,
/// minimizing the squared log error over 13 nodes: within 0.55–1.43x of
/// each but the two of 36,000 rows (1.43x and 2.4x under). Example: states
/// of 300 rows measure 13.1–14.3 µs per group, charged 2,500 + 40 × 300 =
/// 14.5; of 3,000 rows 14.7–27.3 and of 3,600 13.3–32.7, charged 2,500 +
/// 40 × 547 = 24.4 (33.1 when charged per byte).
pub(crate) const KLL_ESTIMATE_OPERATIONS_PER_UNSORTED_ITEM: f64 = 40.0;

/// One group's answer read from any other sketch state, before its output
/// row (`OUTPUT_ROW_OPERATIONS_PER_GROUP`): a DDSketch quantile estimate
/// measures 3.7 µs per group with its row (median of 11 nodes of 1,000
/// groups); `asap_sketchlib`'s own DDSketch quantile takes 0.15 µs and a
/// Count-Min Sketch top-k query 0.5–8 µs.
pub(crate) const SKETCH_ESTIMATE_OPERATIONS_PER_GROUP: f64 = 1_000.0;

/// One group's output row built from its value, with the group's labels
/// and series identity, by a hash aggregate, a finalize or an estimate (a
/// top-k estimate emits `k` rows per group and is charged one): a
/// finalize of exact accumulators measures 2.5 µs
/// per group (median over 135 nodes of 1,000 groups each, 1.8–15 µs). A
/// per-series hash aggregate builds the same rows; its per-row and visit
/// constants are fitted with this charge in place
/// (`OPERATIONS_PER_GROUP_PER_RAW_BATCH`).
pub(crate) const OUTPUT_ROW_OPERATIONS_PER_GROUP: f64 = 2_500.0;

/// Per sketch state a tumbling-window chain completes and emits at the end
/// of each window, beyond its rows' updates: fitting each window's time to
/// its rows and its 1,000 states at 1 Hz (10 s windows of 10,000 rows) and
/// at 10 Hz (60 s windows of 600,000 rows), 2.8 µs per KLL state and
/// 3.4 µs per DDSketch state. Measured apart from
/// `OUTPUT_ROW_OPERATIONS_PER_GROUP` and equal to it: both build one row per
/// group with its labels and series identity, here around a state, there
/// around a value.
const SKETCH_CHAIN_EMIT_OPERATIONS_PER_STATE: f64 = 2_500.0;

/// Per fixed-size exact state a tumbling-window chain emits (sum, count,
/// min, max, rate): 1.9 µs per Rate state, and at ingestion time
/// 1.3–2.1 µs per Max state over 100 to 10,000 series. Less than a sketch
/// state's (`SKETCH_CHAIN_EMIT_OPERATIONS_PER_STATE`): its state is a
/// fixed record, not a sketch to box.
const EXACT_CHAIN_EMIT_OPERATIONS_PER_STATE: f64 = 1_500.0;

/// Per output group of a KLL `SummaryMerge`, beyond
/// `MERGE_OPERATIONS_PER_OUTPUT_GROUP` and its states' merges: creating the
/// group's KLL and moving its merged state out. In the executor, a merge
/// of 5 KLL states of 60 rows per series (1,000 and 10,000 series) measures
/// 32–33 µs per series where its states' merges are charged 14; over 60
/// states per series this adds 4% to the charge, measured 0.8x.
const KLL_MERGE_OPERATIONS_PER_OUTPUT_GROUP: f64 = 19_000.0;

/// The operations one input row costs folded into a state of `summary_type`
/// with `parameters`, which folds `rows_per_state` rows (`None`: unknown)
/// of a column taking `distinct_values` values (`None`: every row's
/// differs). Only a KMV's depends on them
/// ([`kmv_compared_hashes_per_row`]).
pub(crate) fn update_operations_per_row(
    summary_type: &FieldDataType,
    parameters: Option<&SketchParams>,
    rows_per_state: Option<u64>,
    distinct_values: Option<u64>,
) -> f64 {
    match (summary_type, parameters) {
        (FieldDataType::ExactAggregate(kind, _), _) if kind.tracks_counter_increase() => {
            COUNTER_UPDATE_OPERATIONS_PER_ROW
        }
        (FieldDataType::ExactAggregate(..), _) => EXACT_UPDATE_OPERATIONS_PER_ROW,
        (_, Some(SketchParams::DDSketch { .. })) => DDSKETCH_UPDATE_OPERATIONS_PER_ROW,
        (_, Some(SketchParams::Kll { .. })) => KLL_UPDATE_OPERATIONS_PER_ROW,
        (_, Some(SketchParams::Kmv { k })) => {
            KMV_UPDATE_OPERATIONS_PER_ROW
                + KMV_UPDATE_OPERATIONS_PER_COMPARED_HASH
                    * kmv_compared_hashes_per_row(*k, rows_per_state, distinct_values)
        }
        (
            _,
            Some(
                parameters @ (SketchParams::Hydra(_)
                | SketchParams::Cms { .. }
                | SketchParams::CountSketch { .. }
                | SketchParams::CmsWithHeap { .. }
                | SketchParams::CountSketchWithHeap { .. }
                | SketchParams::UnivMon { .. }
                | SketchParams::Hll { .. }),
            ),
        ) => {
            EXACT_UPDATE_OPERATIONS_PER_ROW
                + SKETCH_UPDATE_OPERATIONS_PER_COUNTER * counters_updated_per_row(parameters)
        }
        // A summary without sketch parameters (a sample): one counter.
        (_, None) => EXACT_UPDATE_OPERATIONS_PER_ROW + SKETCH_UPDATE_OPERATIONS_PER_COUNTER,
    }
}

/// The retained hashes one KMV update at `k` compares the row's hash with,
/// averaged over a state's `rows_per_state` rows (`None`: `k` rows), each a
/// draw from `distinct_values` equally likely values (`None`: every row's
/// differs). While the state retains fewer than `k` hashes, every update
/// walks them: a value seen before is found halfway, a new one walks all.
/// Once full, only a hash below the largest retained is walked: a retained
/// value seen before (`k / 2` of them on average) or a new value that
/// enters (all `k`), which after `j` distinct values is `k / j` of the new
/// ones. Averaged over 64 equal slices of the rows; within 1% of a
/// simulation of the same draws. Example: k = 4,323 over 300,000 rows of
/// 1,000 values never fills and compares 500 per row; of 100,000 values,
/// it fills after about 4,400 rows and compares 314.
pub(crate) fn kmv_compared_hashes_per_row(
    k: u32,
    rows_per_state: Option<u64>,
    distinct_values: Option<u64>,
) -> f64 {
    const SLICES: u32 = 64;
    let k = f64::from(k);
    let rows = rows_per_state.map_or(k, |rows| rows as f64);
    let values = distinct_values.map_or(f64::INFINITY, |values| values.max(1) as f64);
    let compared = (0..SLICES)
        .map(|slice| {
            let row = (f64::from(slice) + 0.5) * rows / f64::from(SLICES);
            // Distinct values among the rows before it, and the share of
            // its own value among them.
            let seen = match values.is_finite() {
                true => expected_distinct_values(row, values),
                false => row,
            };
            let seen_before = seen / values;
            match seen < k {
                true => seen_before * seen / 2.0 + (1.0 - seen_before) * seen,
                false => k * k / (2.0 * values) + (1.0 - seen_before) * k * k / seen,
            }
        })
        .sum::<f64>();
    compared / f64::from(SLICES)
}

/// The counters one input row updates in a sketch with `parameters`: one
/// per row of depth for Count-Min and Count Sketch, plus one heap update for
/// their top-k variants; for UnivMon, the same per layer it updates
/// (`univmon_expected_layers`); one for every other sketch. Example: a
/// Count-Min Sketch of depth 3 with a heap updates 4.
pub(crate) fn counters_updated_per_row(parameters: &SketchParams) -> f64 {
    match parameters {
        SketchParams::Hydra(hydra) => counters_updated_per_row(&hydra.per_subpopulation()),
        SketchParams::Cms { depth, .. } | SketchParams::CountSketch { depth, .. } => {
            f64::from(*depth)
        }
        SketchParams::CmsWithHeap { depth, .. }
        | SketchParams::CountSketchWithHeap { depth, .. } => f64::from(*depth) + 1.0,
        SketchParams::UnivMon {
            sketch_rows,
            layers,
            ..
        } => univmon_expected_layers(*layers) * (f64::from(*sketch_rows) + 1.0),
        SketchParams::Kll { .. }
        | SketchParams::DDSketch { .. }
        | SketchParams::Hll { .. }
        | SketchParams::Kmv { .. } => 1.0,
    }
}

/// Layers a UnivMon row updates in expectation. `asap_sketchlib` updates
/// layers `0..=b`, with bottom layer `b` the index of the first zero among
/// hash bits `1..layers`, capped at `layers - 1`; so it reaches layer `j`
/// with probability `2^-j`, and the expectation is
/// `sum_{j < layers} 2^-j = 2 - 2^(1 - layers)`.
fn univmon_expected_layers(layers: u8) -> f64 {
    2.0 - 2f64.powi(1 - i32::from(layers.max(1)))
}

/// The operations one input state of `family` holding `state_bytes` costs
/// merged into its group's state, which holds `merged_state_bytes` once
/// merged; each input state folds `input_rows` (`None`: unknown) and its
/// group merges `input_states_per_group` of them. A KMV merge updates the
/// group's state with each hash the input retains (at most `k`, the
/// distinct values it folded when fewer), as `asap_sketchlib` merges, so
/// each hash costs `KMV_MERGE_OPERATIONS_PER_HASH` plus the retained hashes
/// it is compared with over all the hashes the group's merge inserts
/// ([`kmv_compared_hashes_per_row`]). Each state retains the smallest
/// hashes of the values it folded, so the states draw theirs from the same
/// smallest hashes of the column's values and share some: of `V` values, a
/// state retaining `h` hashes of the `D` it folded draws them from the
/// `V h / D` smallest. Example: a KLL state of 872 bytes merged into one of
/// 33,512 costs 360 + 0.1 × 872 + 0.43 × 33,512 = 14,857. Five 1-minute
/// KMV states at k = 4,323 of 12,000 rows each of 1,000,000 users retain
/// 4,323 hashes each, drawn from the 362,000 smallest: 3.40 ms per state,
/// measured 3.12 on the executor.
pub(crate) fn merge_operations_per_state(
    family: StateFamily<'_>,
    state_bytes: u64,
    merged_state_bytes: u64,
    input_rows: Option<StateInputRows>,
    input_states_per_group: u64,
) -> f64 {
    match family {
        StateFamily::Exact(kind) if kind.tracks_counter_increase() => {
            COUNTER_MERGE_OPERATIONS_PER_STATE
        }
        StateFamily::Exact(ExactKind::Min | ExactKind::Max) => EXTREMUM_MERGE_OPERATIONS_PER_STATE,
        StateFamily::Exact(_) | StateFamily::None => EXACT_MERGE_OPERATIONS_PER_STATE,
        StateFamily::Sketch(SketchParams::DDSketch { .. }) => DDSKETCH_MERGE_OPERATIONS_PER_STATE,
        StateFamily::Sketch(SketchParams::Kll { .. }) => {
            KLL_MERGE_OPERATIONS_PER_STATE
                + KLL_MERGE_OPERATIONS_PER_INPUT_BYTE * state_bytes as f64
                + KLL_MERGE_OPERATIONS_PER_MERGED_BYTE * merged_state_bytes as f64
        }
        StateFamily::Sketch(SketchParams::Kmv { k }) => {
            let values = input_rows.and_then(|rows| rows.distinct_values);
            // The distinct values each input state folded, of which it
            // retains the `k` smallest hashes.
            let folded_values = input_rows.and_then(|rows| rows.rows_per_state).map(|rows| {
                let rows = rows as f64;
                values.map_or(rows, |values| {
                    let values = values.max(1) as f64;
                    expected_distinct_values(rows, values)
                })
            });
            let retained_hashes =
                folded_values.map_or(f64::from(*k), |folded| folded.min(f64::from(*k)));
            let hashes_per_group = retained_hashes * input_states_per_group.max(1) as f64;
            // The smallest hashes of the column's values the states draw
            // theirs from (`None`: every hash differs).
            let shared_hashes = values
                .zip(folded_values)
                .map(|(values, folded)| (values as f64 * retained_hashes / folded.max(1.0)) as u64);
            retained_hashes
                * (KMV_MERGE_OPERATIONS_PER_HASH
                    + KMV_MERGE_OPERATIONS_PER_COMPARED_HASH
                        * kmv_compared_hashes_per_row(
                            *k,
                            Some(hashes_per_group as u64),
                            shared_hashes,
                        ))
        }
        StateFamily::Sketch(
            SketchParams::Hydra(_)
            | SketchParams::Cms { .. }
            | SketchParams::CountSketch { .. }
            | SketchParams::CmsWithHeap { .. }
            | SketchParams::CountSketchWithHeap { .. }
            | SketchParams::UnivMon { .. }
            | SketchParams::Hll { .. },
        ) => {
            EXACT_MERGE_OPERATIONS_PER_STATE + SKETCH_MERGE_OPERATIONS_PER_BYTE * state_bytes as f64
        }
    }
}

/// The operations a `SummaryMerge` of `family` with `output_groups` groups
/// costs beyond its input states' merges ([`merge_operations_per_state`]):
/// each group's state and output row. Example: 1,000 groups cost
/// 1,100,000, of KLL states 20,100,000.
pub(crate) fn merge_output_operations(family: StateFamily<'_>, output_groups: u64) -> f64 {
    let per_group = match family {
        StateFamily::Sketch(SketchParams::Kll { .. }) => {
            MERGE_OPERATIONS_PER_OUTPUT_GROUP + KLL_MERGE_OPERATIONS_PER_OUTPUT_GROUP
        }
        StateFamily::Sketch(
            SketchParams::DDSketch { .. }
            | SketchParams::Hydra(_)
            | SketchParams::Cms { .. }
            | SketchParams::CountSketch { .. }
            | SketchParams::CmsWithHeap { .. }
            | SketchParams::CountSketchWithHeap { .. }
            | SketchParams::UnivMon { .. }
            | SketchParams::Hll { .. }
            | SketchParams::Kmv { .. },
        )
        | StateFamily::Exact(_)
        | StateFamily::None => MERGE_OPERATIONS_PER_OUTPUT_GROUP,
    };
    output_groups as f64 * per_group
}

/// The operations a `SummaryMerge` that reads one state per group costs,
/// of any family: each state passes through to its group's slot unmerged
/// (the executor keeps a group's first state as it is), so the merge pays
/// each group's row (`MERGE_OPERATIONS_PER_OUTPUT_GROUP`) and no state's
/// merge. Merging the one window a time range selects from a chain of
/// 1,000 states per window measures 0.94–1.0 ms for KLL states (k = 547),
/// 0.97 for DDSketch states and 0.87–1.31 for exact accumulators, where a
/// KLL merge was charged 29 ms. The time per group grows with the
/// groups: at 10,000 groups such merges measure 1.5–1.8 µs per group, a
/// median 1.38–1.62x their charge (up to 2.29x for KLL states), at 100 and
/// 1,000 groups 0.96–1.20x (`MERGE_OPERATIONS_PER_OUTPUT_GROUP`). Example:
/// 1,000 groups cost 1,100,000.
pub(crate) fn pass_through_merge_operations(output_groups: u64) -> f64 {
    output_groups as f64 * MERGE_OPERATIONS_PER_OUTPUT_GROUP
}

/// The operations per state a tumbling-window chain of `summary_type`
/// costs to complete and emit each window's state: 1,500 for an exact
/// accumulator, 2,500 for a sketch or any other summary.
pub(crate) fn chain_emit_operations_per_state(summary_type: &FieldDataType) -> f64 {
    match summary_type {
        FieldDataType::ExactAggregate(..) => EXACT_CHAIN_EMIT_OPERATIONS_PER_STATE,
        FieldDataType::Sketch(..)
        | FieldDataType::Plain(..)
        | FieldDataType::SummarySample(..)
        | FieldDataType::Wavelet(..)
        | FieldDataType::StatModel(..) => SKETCH_CHAIN_EMIT_OPERATIONS_PER_STATE,
    }
}

/// How the summary state an estimate reads was last written, which a
/// KLL's read depends on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StateLastWrittenBy {
    /// A merge, which leaves a KLL's levels sorted.
    Merge,
    /// Updates of `rows_per_state` rows each (`None` when unknown), whose
    /// newest items a KLL sorts when read.
    Updates { rows_per_state: Option<u64> },
}

/// The operations reading one group's answer from a state of `family`
/// holding `state_bytes`, last written by `written_by`, costs, its output
/// row included. A KLL state last written by a merge is read per byte; one
/// built by updates per unsorted item, at most its `k` (every one when its
/// rows are unknown). Example: a DDSketch quantile costs 1,000 + 2,500 =
/// 3,500 per group; a KLL at k = 547 of 4,844 bytes 6,860 after a merge
/// and, built from 300 rows, 2,500 + 40 × 300 = 14,500 after updates.
pub(crate) fn estimate_operations_per_group(
    family: StateFamily<'_>,
    state_bytes: u64,
    written_by: StateLastWrittenBy,
) -> f64 {
    let read = match family {
        StateFamily::Sketch(SketchParams::Kll { k }) => match written_by {
            StateLastWrittenBy::Merge => KLL_ESTIMATE_OPERATIONS_PER_BYTE * state_bytes as f64,
            StateLastWrittenBy::Updates { rows_per_state } => {
                let k = u64::from(*k);
                let unsorted_items = rows_per_state.map_or(k, |rows| rows.min(k));
                KLL_ESTIMATE_OPERATIONS_PER_UNSORTED_ITEM * unsorted_items as f64
            }
        },
        StateFamily::Sketch(
            SketchParams::DDSketch { .. }
            | SketchParams::Hydra(_)
            | SketchParams::Cms { .. }
            | SketchParams::CountSketch { .. }
            | SketchParams::CmsWithHeap { .. }
            | SketchParams::CountSketchWithHeap { .. }
            | SketchParams::UnivMon { .. }
            | SketchParams::Hll { .. }
            | SketchParams::Kmv { .. },
        ) => SKETCH_ESTIMATE_OPERATIONS_PER_GROUP,
        StateFamily::Exact(_) | StateFamily::None => 0.0,
    };
    read + OUTPUT_ROW_OPERATIONS_PER_GROUP
}

/// The operations one input row costs a hash aggregate with
/// `grouping_key_count` keys and `measures` reading `rows` rows into
/// `group_count` groups, whose distinct counts and frequency statistics
/// read columns taking `distinct_values` values: the first key interned
/// and its group read, each further key interned and the tuple's group
/// found, each measure folded (at least one operation for an aggregate
/// without measures). Example: a per-series `max_over_time` (one key, one
/// measure) costs 16 + 4 = 20; `GROUP BY job, instance` with one measure
/// 28 more.
pub(crate) fn hash_aggregate_operations_per_row(
    grouping_key_count: u64,
    measures: &[AggIntent],
    rows: u64,
    group_count: u64,
    distinct_values: u64,
) -> f64 {
    let rows_per_group = rows.div_ceil(group_count.max(1));
    let keys = match grouping_key_count {
        0 => 0.0,
        keys => {
            HASH_AGGREGATE_OPERATIONS_PER_ROW
                + (keys - 1) as f64 * HASH_AGGREGATE_OPERATIONS_PER_FURTHER_KEY
        }
    };
    keys + measures
        .iter()
        .map(|measure| {
            aggregate_measure_operations_per_row(
                measure,
                rows,
                group_count,
                rows_per_group,
                distinct_values,
            )
        })
        .sum::<f64>()
        .max(1.0)
}

/// The operations one input row costs a hash aggregate's `measure` over
/// `rows` rows in `group_count` groups of `rows_per_group`, an estimate of
/// its work:
/// - an exact quantile keeps the row and sorts its group
///   (`log2(rows_per_group)` levels);
/// - an exact distinct count or frequency statistic interns the row's value
///   in a table of every distinct value read and keeps its group's
///   (group, value) pairs ([`exact_distinct_value_operations_per_row`] of
///   the `distinct_values` the column takes); a top-k hashes the row's
///   value into its group's map, the work a further grouping key does
///   (`HASH_AGGREGATE_OPERATIONS_PER_FURTHER_KEY`, not measured);
/// - a counter's rate or increase, or a regression over a series' samples
///   (`deriv`, `predict_linear`), does about what the Rate accumulator does
///   per row (`COUNTER_UPDATE_OPERATIONS_PER_ROW`): it reads each sample's
///   time and value in order and keeps running terms;
/// - any other folds the value (`AGGREGATE_MEASURE_OPERATIONS_PER_ROW`),
///   including `changes`, `delta`, `idelta` and `resets`, which compare
///   neighbouring samples without tracking a counter's increase (not
///   measured).
fn aggregate_measure_operations_per_row(
    measure: &AggIntent,
    rows: u64,
    group_count: u64,
    rows_per_group: u64,
    distinct_values: u64,
) -> f64 {
    match measure {
        AggIntent::Quantile { .. } => {
            AGGREGATE_MEASURE_OPERATIONS_PER_ROW
                + (rows_per_group.max(2) as f64).log2() * SORT_OPERATIONS_PER_ROW_PER_LEVEL
        }
        AggIntent::Cardinality { .. }
        | AggIntent::FrequencyL2 { .. }
        | AggIntent::FrequencyEntropy { .. } => {
            AGGREGATE_MEASURE_OPERATIONS_PER_ROW
                + exact_distinct_value_operations_per_row(distinct_values, group_count, rows)
        }
        AggIntent::TopK { .. } => {
            AGGREGATE_MEASURE_OPERATIONS_PER_ROW + HASH_AGGREGATE_OPERATIONS_PER_FURTHER_KEY
        }
        AggIntent::Rate
        | AggIntent::IRate
        | AggIntent::Increase
        | AggIntent::Deriv
        | AggIntent::PredictLinear { .. } => COUNTER_UPDATE_OPERATIONS_PER_ROW,
        AggIntent::Count
        | AggIntent::Sum { .. }
        | AggIntent::Min { .. }
        | AggIntent::Max { .. }
        | AggIntent::Avg { .. }
        | AggIntent::StdDev { .. }
        | AggIntent::Variance { .. }
        | AggIntent::PearsonCorr { .. }
        | AggIntent::Changes
        | AggIntent::Delta
        | AggIntent::IDelta
        | AggIntent::Resets
        | AggIntent::DoubleExpSmoothing { .. }
        | AggIntent::HistogramCount
        | AggIntent::HistogramSum
        | AggIntent::HistogramAvg
        | AggIntent::HistogramStdDev
        | AggIntent::HistogramStdVar
        | AggIntent::HistogramFraction { .. }
        | AggIntent::HistogramQuantile { .. }
        | AggIntent::Math(_)
        | AggIntent::Absent { .. }
        | AggIntent::AbsentOverTime
        | AggIntent::PresentOverTime
        | AggIntent::TimeFn(_)
        | AggIntent::Group
        | AggIntent::CountValues { .. }
        | AggIntent::LastOverTime
        | AggIntent::FirstOverTime
        | AggIntent::MadOverTime
        | AggIntent::TsOfMinOverTime
        | AggIntent::TsOfMaxOverTime
        | AggIntent::TsOfFirstOverTime
        | AggIntent::TsOfLastOverTime
        | AggIntent::Extension { .. } => AGGREGATE_MEASURE_OPERATIONS_PER_ROW,
    }
}
