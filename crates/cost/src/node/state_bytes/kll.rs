//! The bytes a KLL state holds: sealed in its view layout, or as an owned
//! `asap_sketchlib` sketch still receiving values.

use super::StateInputRows;
use asap_types::summary_type::KLL_MAX_K;

/// Levels `asap_sketchlib`'s KLL has room for (`MAX_LEVELS`).
const KLL_MAX_LEVELS: usize = 61;

/// `asap_sketchlib`'s smallest level capacity, `m` (`DEFAULT_M`, the `m` of
/// the executor's KLL).
const KLL_MIN_LEVEL_CAPACITY: u64 = 8;

/// Level capacities `asap_sketchlib` caches by height from the top
/// (`CAPACITY_CACHE_LEN`); every greater height takes the last.
const KLL_CAPACITY_HEIGHTS: usize = 20;

/// Bytes of a KLL view's header (`Kll::to_view_bytes`): its magic and
/// version (8), and `k`, `m`, its levels and its items (8 each).
const KLL_VIEW_HEADER_BYTES: u64 = 40;

/// Rows up to which [`kll_retained_levels_and_items`] runs the compaction schedule; past
/// them a state is priced at the capacity of its levels, a bound.
/// 2^20 rows run about 10^5 compactions per seed.
const KLL_SIMULATED_ROWS_MAX: u64 = 1 << 20;

/// Coin seeds [`kll_retained_levels_and_items`] runs the schedule with (1 to 16), the most
/// items of any taken: enough that it bounds the mean state of a column
/// (at 1,100 rows and `k` = 200 a state retains 183 to 186 items; four
/// seeds leave at most 185, sixteen 186).
const KLL_SIMULATED_SEEDS: std::ops::RangeInclusive<u64> = 1..=16;

/// Bytes a sealed KLL of `k` holds after folding `input_rows`, as the
/// executor keeps it: in its view layout (`Kll::to_view_bytes`), a 40-byte
/// header, its `levels + 1` level boundaries (4 bytes each, padded to 8) and
/// its retained 8-byte items. The state's 4-byte offset in its column is a
/// byte of its row beside the state (`state_offset_bytes_per_row`).
///
/// Up to `k` rows nothing compacts: one level and every row. Past `k`, the
/// levels and items [`kll_retained_levels_and_items`] gives. Unknown rows fill every level
/// at its capacity. A state is assumed to fold at least one row.
///
/// Example: `k` = 132 over a 15 s window of one sample a second (15 rows):
/// 40 + 8 + 8 · 15 = 168 bytes.
pub(super) fn kll_view_bytes(k: u32, input_rows: StateInputRows) -> u64 {
    let k = u64::from(k.clamp(8, KLL_MAX_K));
    let (levels, items) = match input_rows.rows_per_state() {
        None => (KLL_MAX_LEVELS as u64, kll_capacity_items(k, KLL_MAX_LEVELS)),
        Some(rows) if rows <= k => (1, rows.max(1)),
        Some(rows) => kll_retained_levels_and_items(k, rows),
    };
    KLL_VIEW_HEADER_BYTES + (4 * (levels + 1)).div_ceil(8) * 8 + 8 * items
}

/// Bytes an owned KLL of `k` (a state still receiving values) is priced at
/// after folding `input_rows`, as before sealing states into views: the struct,
/// `levels + 1` 8-byte level boundaries for the `L(n)` levels `n` rows fill
/// (`kll_levels_after`), and its 8-byte items. Up to `k` rows, exactly the
/// `n` items, as a shrunk sketch (`Kll::shrink_to_fit`) holds them; an
/// open sketch holds more (62 level slots and a buffer doubling from 16
/// items), which this leaves uncharged. Past
/// `k`, the buffer an open sketch grows to (16 items doubled up to the
/// capacity of all its levels, `kll_buffer_items`), what a merge holds
/// while it folds windows. Unknown rows fill every level.
///
/// Example: `k` = 132 over 15 rows: 248 + 8 · 2 + 8 · 15 = 384 bytes.
pub(super) fn kll_owned_bytes(k: u32, input_rows: StateInputRows) -> u64 {
    let k = u64::from(k.clamp(8, KLL_MAX_K));
    let (levels, items) = match input_rows.rows_per_state() {
        None => (KLL_MAX_LEVELS as u64, kll_capacity_items(k, KLL_MAX_LEVELS)),
        Some(rows) => {
            let rows = rows.max(1);
            let items = match rows <= k {
                true => rows,
                false => kll_buffer_items(k, rows),
            };
            (kll_levels_after(k, rows), items)
        }
    };
    std::mem::size_of::<asap_sketchlib::Kll<f64>>() as u64 + 8 * (levels + 1) + 8 * items
}

/// The levels and the items a KLL of `k` retains after `rows` values, by
/// `asap_sketchlib`'s compaction schedule run on the levels' sizes alone:
/// level 0 compacts once it holds more than its capacity, a compaction
/// keeps half of its items, rounded up or down by a coin toss when odd, and
/// moves them up a level, which compacts in turn while over its capacity; a
/// level is added when the top compacts. Capacities are
/// `max(ceil(k · (2/3)^i), m)` by height `i` from the top.
///
/// The coin makes the count random (over 50 states of 13,200 rows at
/// `k` = 200, 240 to 298 items): the schedule runs with each of
/// [`KLL_SIMULATED_SEEDS`] (the library's own coin, `Kll::with_seed`), and
/// the most items any leaves is taken. Past [`KLL_SIMULATED_ROWS_MAX`] rows,
/// the capacity of the levels the rows fill bounds it.
///
/// Results are cached by `(k, rows)`: planning prices the same states many
/// times, and near [`KLL_SIMULATED_ROWS_MAX`] one run of all seeds takes
/// about 20 ms.
fn kll_retained_levels_and_items(k: u64, rows: u64) -> (u64, u64) {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    /// Levels and items retained, by `(k, rows)`.
    type RetainedByShape = HashMap<(u64, u64), (u64, u64)>;
    static RETAINED: OnceLock<Mutex<RetainedByShape>> = OnceLock::new();
    if rows > KLL_SIMULATED_ROWS_MAX {
        let levels = kll_levels_after(k, rows);
        return (levels, kll_capacity_items(k, levels as usize));
    }
    let cache = RETAINED.get_or_init(Default::default);
    if let Some(&retained) = cache.lock().expect("not poisoned").get(&(k, rows)) {
        return retained;
    }
    let retained = KLL_SIMULATED_SEEDS
        .map(|seed| kll_retained_with_seed(k, rows, seed))
        .max_by_key(|&(_, items)| items)
        .expect("seeds");
    cache
        .lock()
        .expect("not poisoned")
        .insert((k, rows), retained);
    retained
}

/// [`kll_retained_levels_and_items`] for one coin `seed`: exactly what
/// `asap_sketchlib::Kll::with_seed(k, 8, seed)` retains after `rows`
/// updates.
fn kll_retained_with_seed(k: u64, rows: u64, seed: u64) -> (u64, u64) {
    let capacities = kll_capacities(k);
    let capacity = |levels: usize, level: usize| {
        capacities[(levels - 1 - level).min(KLL_CAPACITY_HEIGHTS - 1)]
    };
    let mut coin = KllCoin::from_seed(seed);
    let mut sizes = [0u64; KLL_MAX_LEVELS + 1];
    let mut levels = 1;
    let mut remaining = rows;
    loop {
        // The values until level 0 holds one more than its capacity.
        let fill = (capacity(levels, 0) + 1).saturating_sub(sizes[0]).max(1);
        if remaining < fill {
            sizes[0] += remaining;
            break;
        }
        remaining -= fill;
        sizes[0] += fill;
        let mut level = 0;
        while level < KLL_MAX_LEVELS && sizes[level] > capacity(levels, level) {
            if level + 1 == levels {
                levels += 1;
            }
            let survivors = (sizes[level] - u64::from(coin.toss())).div_ceil(2);
            sizes[level] = 0;
            sizes[level + 1] += survivors;
            level += 1;
        }
    }
    (levels as u64, sizes[..levels].iter().sum())
}

/// `asap_sketchlib`'s KLL compaction coin (`Coin`): xorshift-multiply
/// draws, consumed one bit at a time.
struct KllCoin {
    state: u64,
    bits: u64,
    remaining_bits: u32,
}

impl KllCoin {
    /// A seed of 0 is replaced, as the library replaces it.
    fn from_seed(seed: u64) -> Self {
        Self {
            state: Self::nonzero(seed),
            bits: 0,
            remaining_bits: 0,
        }
    }

    fn nonzero(state: u64) -> u64 {
        match state {
            0 => 0x9e37_79b9_7f4a_7c15,
            state => state,
        }
    }

    fn toss(&mut self) -> bool {
        if self.remaining_bits == 0 {
            let mut x = self.state;
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            self.state = Self::nonzero(x.wrapping_mul(2_685_821_657_736_338_717));
            self.bits = self.state;
            self.remaining_bits = u64::BITS;
        }
        let bit = self.bits & 1 != 0;
        self.bits >>= 1;
        self.remaining_bits -= 1;
        bit
    }
}

/// Level capacities of a KLL of `k` by height from the top:
/// `max(ceil(k · (2/3)^i), m)` (`asap_sketchlib`'s `capacity_table`).
fn kll_capacities(k: u64) -> [u64; KLL_CAPACITY_HEIGHTS] {
    let mut capacities = [0; KLL_CAPACITY_HEIGHTS];
    let mut scale = 1.0_f64;
    for capacity in capacities.iter_mut() {
        *capacity = ((k as f64 * scale).ceil() as u64).max(KLL_MIN_LEVEL_CAPACITY);
        scale *= 2.0 / 3.0;
    }
    capacities
}

/// Items an open `asap_sketchlib` KLL of `k` allocates for `rows`: its
/// item array starts empty and grows when full, to at least twice its
/// length and at least 16 items, at most the capacity of all its levels
/// ([`kll_capacity_items`]): the smallest `16 · 2^j` ≥ `rows`, capped.
fn kll_buffer_items(k: u64, rows: u64) -> u64 {
    const INITIAL_BUFFER_ITEMS: u64 = 16;
    rows.div_ceil(INITIAL_BUFFER_ITEMS)
        .next_power_of_two()
        .saturating_mul(INITIAL_BUFFER_ITEMS)
        .min(kll_capacity_items(k, KLL_MAX_LEVELS))
}

/// `L(n)`: the levels a KLL of `k` holds after `n` rows, a level added once
/// the top holds `k + 1` items of weight 2^top (`asap_sketchlib`'s
/// `levels_after`). 1 up to `k` rows; 2 from `k + 1`.
fn kll_levels_after(k: u64, rows: u64) -> u64 {
    let mut levels = 1;
    while levels < KLL_MAX_LEVELS as u64
        && 1u64
            .checked_shl(levels as u32 - 1)
            .and_then(|weight| weight.checked_mul(k + 1))
            .is_some_and(|items| items <= rows)
    {
        levels += 1;
    }
    levels
}

/// The items the top `levels` levels of a KLL of `k` hold at capacity:
/// `max(ceil(k · (2/3)^i), 8)` for heights `i` from the top. Over all
/// [`KLL_MAX_LEVELS`] levels, the most items the library holds: 1,206 at
/// `k` = 269.
fn kll_capacity_items(k: u64, levels: usize) -> u64 {
    let capacities = kll_capacities(k);
    (0..levels)
        .map(|height| capacities[height.min(KLL_CAPACITY_HEIGHTS - 1)])
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use asap_sketchlib::{Kll, KllView};

    /// The compaction schedule run on level sizes retains exactly the
    /// levels and items the library's KLL of the same coin seed retains,
    /// below, at and past `k` and across several level additions.
    #[test]
    fn the_schedule_retains_what_the_library_retains() {
        for k in [8u64, 200, 1_000] {
            for rows in [1u64, 15, 200, 201, 1_100, 2_000, 13_200, 100_000] {
                for seed in [1u64, 7, 12_345] {
                    let mut sketch = Kll::<f64>::with_seed(k as usize, 8, seed);
                    for i in 0..rows {
                        sketch.update(&(i as f64 * 0.37 % 101.0));
                    }
                    let bytes = sketch.to_view_bytes();
                    KllView::from_bytes(&bytes).unwrap();
                    let held = (
                        u64::from_le_bytes(bytes[24..32].try_into().unwrap()),
                        u64::from_le_bytes(bytes[32..40].try_into().unwrap()),
                    );
                    assert_eq!(
                        kll_retained_with_seed(k, rows, seed),
                        held,
                        "k {k}, {rows} rows, seed {seed}"
                    );
                }
            }
        }
    }

    /// A view's bytes: its header, its level boundaries padded to 8 and
    /// its items, as `Kll::to_view_bytes` writes them.
    #[test]
    fn kll_view_bytes_are_the_views_length() {
        let rows = |n: u32| StateInputRows {
            folded_rows_per_state: Some(f64::from(n)),
            non_negative: true,
            value_magnitude_range: None,
            distinct_values: None,
        };
        for n in [1u32, 15, 200] {
            let mut sketch = Kll::<f64>::with_seed(200, 8, 1);
            for i in 0..n {
                sketch.update(&f64::from(i));
            }
            assert_eq!(
                kll_view_bytes(200, rows(n)),
                sketch.to_view_bytes().len() as u64,
                "{n} rows"
            );
        }
        assert_eq!(kll_view_bytes(132, rows(15)), 168);
        assert_eq!(kll_owned_bytes(132, rows(15)), 384);
    }
}
