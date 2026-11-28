//! The bytes a KLL state holds: sealed in its view layout, or as an open
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

/// Rows up to which [`kll_held_after`] runs the compaction schedule; past
/// them a state is priced at the capacity of its levels, a bound.
/// 2^20 rows run about 10^5 compactions per seed.
const KLL_SIMULATED_ROWS_MAX: u64 = 1 << 20;

/// Coin seeds [`kll_held_after`] runs the schedule with (1 to 4), taking
/// the most any seed leaves: enough that it bounds the mean state of a column
/// (at 1,100 rows and `k` = 200 a state retains 183 to 186 items; four
/// seeds leave at most 185, sixteen 186). Each seed is one run of the
/// schedule, so more seeds cost planning time for little change.
pub(super) const KLL_SIMULATED_SEEDS: std::ops::RangeInclusive<u64> = 1..=4;

/// Bytes a sealed KLL of `k` holds after folding `input_rows`, as the
/// executor keeps it: in its view layout (`Kll::to_view_bytes`), a 40-byte
/// header, its `levels + 1` level boundaries (4 bytes each, padded to 8) and
/// its retained 8-byte items. The state's 4-byte offset in its column is a
/// byte of its row beside the state (`state_offset_bytes_per_row`).
///
/// Up to `k` rows nothing compacts: one level and every row. Past `k`, the
/// levels and items [`kll_held_after`] gives. Unknown rows fill every level
/// at its capacity. A state is assumed to fold at least one row.
///
/// Example: `k` = 132 over a 15 s window of one sample a second (15 rows):
/// 40 + 8 + 8 · 15 = 168 bytes.
pub(super) fn kll_view_bytes(k: u32, input_rows: StateInputRows) -> u64 {
    let k = u64::from(k.clamp(8, KLL_MAX_K));
    let (levels, items) = match input_rows.rows_per_state() {
        None => (KLL_MAX_LEVELS as u64, kll_capacity_items(k, KLL_MAX_LEVELS)),
        Some(rows) if rows <= k => (1, rows.max(1)),
        Some(rows) => {
            let held = kll_held_after(k, rows);
            (held.levels, held.retained_items)
        }
    };
    KLL_VIEW_HEADER_BYTES + (4 * (levels + 1)).div_ceil(8) * 8 + 8 * items
}

/// Bytes an open KLL of `k` (a state still receiving values: a chain's
/// current window, a query-time build before it seals, a merge target)
/// holds after folding `input_rows`, as `Kll::memory_bytes` counts an
/// unshrunk sketch: the struct, a slot for every one of its
/// `KLL_MAX_LEVELS + 1` level boundaries, its item buffer and its
/// compaction scratch. Up to `k` rows nothing compacts: the buffer is the
/// smallest `16 · 2^j` items holding every row, and no scratch. Past `k`,
/// the buffer and scratch [`kll_held_after`] gives. Unknown rows: the
/// largest buffer ([`kll_max_capacity`]) and the scratch bound
/// ([`kll_scratch_bound_items`]). A memoized CDF, held only between a query
/// and the next update, is left out.
///
/// Example: `k` = 132 over 15 rows: 248 + 8 · 62 + 8 · 16 = 872 bytes.
pub(super) fn kll_open_bytes(k: u32, input_rows: StateInputRows) -> u64 {
    let k = u64::from(k.clamp(8, KLL_MAX_K));
    let (buffer_items, scratch_items) = match input_rows.rows_per_state() {
        None => (kll_max_capacity(k), kll_scratch_bound_items(k)),
        Some(rows) if rows <= k => (kll_buffer_items(k, rows.max(1)), 0),
        Some(rows) => {
            let held = kll_held_after(k, rows);
            (held.buffer_items, held.scratch_items)
        }
    };
    kll_open_bytes_holding(buffer_items, scratch_items)
}

/// Bytes `Kll::memory_bytes` counts for an unshrunk KLL whose buffer has
/// room for `buffer_items` and whose compaction scratch for
/// `scratch_items`: its struct (248), its `KLL_MAX_LEVELS + 1` level slots
/// (8 each) and those 8-byte items.
fn kll_open_bytes_holding(buffer_items: u64, scratch_items: u64) -> u64 {
    std::mem::size_of::<asap_sketchlib::Kll<f64>>() as u64
        + 8 * (KLL_MAX_LEVELS as u64 + 1)
        + 8 * (buffer_items + scratch_items)
}

/// What a KLL holds after a stream of rows, in counts of 8-byte items
/// (levels excepted), by [`kll_held_after`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct KllHeld {
    /// Levels in use.
    levels: u64,
    /// Items retained over those levels: what its view holds.
    retained_items: u64,
    /// Items its buffer has room for: the smallest `16 · 2^j` holding the
    /// most items it ever retained at once, at most `max_capacity`
    /// (`kll_buffer_items`).
    buffer_items: u64,
    /// Items its compaction scratch (`merge_buf`) has room for.
    scratch_items: u64,
}

impl KllHeld {
    /// The levels and items of whichever retains more items (`other`'s on
    /// a tie), and the larger buffer and the larger scratch.
    fn upper_bound_with(self, other: Self) -> Self {
        let retains_more = match other.retained_items >= self.retained_items {
            true => other,
            false => self,
        };
        Self {
            buffer_items: self.buffer_items.max(other.buffer_items),
            scratch_items: self.scratch_items.max(other.scratch_items),
            ..retains_more
        }
    }
}

/// What a KLL of `k` holds after `rows` values, by `asap_sketchlib`'s
/// compaction schedule run on the levels' sizes alone: level 0 compacts
/// once it holds more than its capacity, a compaction keeps half of its
/// items, rounded up or down by a coin toss when odd, and moves them up a
/// level, which compacts in turn while over its capacity; a level is added
/// when the top compacts. Capacities are `max(ceil(k · (2/3)^i), m)` by
/// height `i` from the top.
///
/// The coin makes the counts random (over 50 states of 13,200 rows at
/// `k` = 200, 240 to 298 items): the schedule runs with each of
/// [`KLL_SIMULATED_SEEDS`] (the library's own coin, `Kll::with_seed`):
/// the levels and items of the seed retaining the most items, and the
/// largest buffer and scratch of any seed ([`KllHeld::upper_bound_with`]). Past
/// [`KLL_SIMULATED_ROWS_MAX`] rows, the capacity of the levels the rows
/// fill bounds the levels and items, the buffer is the largest
/// ([`kll_max_capacity`]) and the scratch its bound
/// ([`kll_scratch_bound_items`]).
///
/// Results are cached by `(k, rows)`: planning prices the same states many
/// times, and near [`KLL_SIMULATED_ROWS_MAX`] one run of all seeds takes
/// about 8 ms.
fn kll_held_after(k: u64, rows: u64) -> KllHeld {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    static SCHEDULES: OnceLock<Mutex<HashMap<(u64, u64), KllHeld>>> = OnceLock::new();
    if rows > KLL_SIMULATED_ROWS_MAX {
        let levels = kll_levels_after(k, rows);
        return KllHeld {
            levels,
            retained_items: kll_capacity_items(k, levels as usize),
            buffer_items: kll_max_capacity(k),
            scratch_items: kll_scratch_bound_items(k),
        };
    }
    let cache = SCHEDULES.get_or_init(Default::default);
    if let Some(&held) = cache.lock().expect("not poisoned").get(&(k, rows)) {
        return held;
    }
    let held = KLL_SIMULATED_SEEDS
        .map(|seed| kll_held_after_with_seed(k, rows, seed))
        .fold(KllHeld::default(), KllHeld::upper_bound_with);
    cache.lock().expect("not poisoned").insert((k, rows), held);
    held
}

/// [`kll_held_after`] for one coin `seed`: exactly the levels, items and
/// buffer `asap_sketchlib::Kll::with_seed(k, 8, seed)` holds after `rows`
/// updates, and its scratch when every compaction merges its survivors
/// into the level above (it skips that merge when the two runs are
/// already in order, so it may hold less).
fn kll_held_after_with_seed(k: u64, rows: u64, seed: u64) -> KllHeld {
    let capacities = kll_capacities(k);
    let capacity = |levels: usize, level: usize| {
        capacities[(levels - 1 - level).min(KLL_CAPACITY_HEIGHTS - 1)]
    };
    let mut coin = KllCoin::from_seed(seed);
    let mut sizes = [0u64; KLL_MAX_LEVELS + 1];
    let mut levels = 1;
    let mut remaining = rows;
    // The items held now, and the most held at once (just before a
    // compaction, or at the end).
    let mut retained_items = 0;
    let mut most_retained = 0;
    let mut scratch_items = 0;
    loop {
        // The values until level 0 holds one more than its capacity.
        let fill = (capacity(levels, 0) + 1).saturating_sub(sizes[0]).max(1);
        if remaining < fill {
            retained_items += remaining;
            break;
        }
        remaining -= fill;
        sizes[0] += fill;
        retained_items += fill;
        most_retained = most_retained.max(retained_items);
        let mut level = 0;
        while level < KLL_MAX_LEVELS && sizes[level] > capacity(levels, level) {
            if level + 1 == levels {
                levels += 1;
            }
            let survivors = (sizes[level] - u64::from(coin.toss())).div_ceil(2);
            // The survivors merge into the level above through the
            // scratch, which copies the shorter run and grows as a `Vec`
            // does: to at least twice its room and at least 4 items.
            let copied = survivors.min(sizes[level + 1]);
            if copied > scratch_items {
                scratch_items = copied.max(2 * scratch_items).max(4);
            }
            retained_items -= sizes[level] - survivors;
            sizes[level] = 0;
            sizes[level + 1] += survivors;
            level += 1;
        }
    }
    KllHeld {
        levels: levels as u64,
        retained_items,
        buffer_items: kll_buffer_items(k, most_retained.max(retained_items)),
        scratch_items,
    }
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

/// The capacity of each level of a KLL of `k` by height `i` from the top,
/// without end: `max(ceil(k · (2/3)^i), m)`, the scale multiplied by 2/3
/// per height as `asap_sketchlib` does.
fn kll_capacity_by_height(k: u64) -> impl Iterator<Item = u64> {
    std::iter::successors(Some(1.0_f64), |scale| Some(scale * (2.0 / 3.0)))
        .map(move |scale| ((k as f64 * scale).ceil() as u64).max(KLL_MIN_LEVEL_CAPACITY))
}

/// Level capacities of a KLL of `k` as `asap_sketchlib` caches them by
/// height from the top (`capacity_table`).
fn kll_capacities(k: u64) -> [u64; KLL_CAPACITY_HEIGHTS] {
    let mut capacities = [0; KLL_CAPACITY_HEIGHTS];
    for (capacity, at_height) in capacities.iter_mut().zip(kll_capacity_by_height(k)) {
        *capacity = at_height;
    }
    capacities
}

/// Items the buffer of an open `asap_sketchlib` KLL of `k` has room for
/// once it has held `items` at once: it starts empty and grows when full,
/// to at least twice its length and at least 16 items, at most the
/// library's `max_capacity` ([`kll_max_capacity`]): the smallest
/// `16 · 2^j` ≥ `items`, capped. Example: 100 items, 128.
fn kll_buffer_items(k: u64, items: u64) -> u64 {
    const INITIAL_BUFFER_ITEMS: u64 = 16;
    items
        .div_ceil(INITIAL_BUFFER_ITEMS)
        .checked_next_power_of_two()
        .map_or(u64::MAX, |blocks| {
            blocks.saturating_mul(INITIAL_BUFFER_ITEMS)
        })
        .min(kll_max_capacity(k))
}

/// `asap_sketchlib`'s `max_capacity` of a KLL of `k` (`compute_max_capacity`):
/// `max(ceil(k · (2/3)^i), m)` summed over all [`KLL_MAX_LEVELS`] heights,
/// the most its buffer grows to. Equals [`kll_capacity_items`] over every
/// level up to `k` of about 17,700, where the cached capacities have not
/// yet all decayed to `m`.
fn kll_max_capacity(k: u64) -> u64 {
    kll_capacity_by_height(k).take(KLL_MAX_LEVELS).sum()
}

/// Items of compaction scratch an open KLL of `k` holds at most, the bound
/// `asap_sketchlib`'s `Kll::theoretical_memory_bytes` takes: `2 (k - 1)` (a
/// compaction copies at most `k` items, and the scratch grows to at most
/// about twice that).
fn kll_scratch_bound_items(k: u64) -> u64 {
    2 * (k - 1)
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
/// `max(ceil(k · (2/3)^i), 8)` for heights `i` from the top, every height
/// past the cached ones at the last cached capacity. Over all
/// [`KLL_MAX_LEVELS`] levels, 1,206 at `k` = 269: the most items the
/// library holds ([`kll_max_capacity`]) for `k` up to about 17,700.
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

    /// The compaction schedule run on level sizes holds exactly what the
    /// library's KLL of the same coin seed holds, below, at and past `k`
    /// and across several level additions: its view's levels and items,
    /// and, open (`memory_bytes`, unshrunk), its buffer and scratch.
    #[test]
    fn the_schedule_holds_what_the_library_holds() {
        let open_bytes =
            |held: KllHeld| kll_open_bytes_holding(held.buffer_items, held.scratch_items);
        for k in [8u64, 200, 1_000] {
            for rows in [1u64, 15, 200, 201, 1_100, 2_000, 13_200, 100_000] {
                for seed in [1u64, 7, 12_345] {
                    let mut sketch = Kll::<f64>::with_seed(k as usize, 8, seed);
                    for i in 0..rows {
                        sketch.update(&(i as f64 * 0.37 % 101.0));
                    }
                    let bytes = sketch.to_view_bytes();
                    KllView::from_bytes(&bytes).unwrap();
                    let held = kll_held_after_with_seed(k, rows, seed);
                    assert_eq!(
                        (held.levels, held.retained_items),
                        (
                            u64::from_le_bytes(bytes[24..32].try_into().unwrap()),
                            u64::from_le_bytes(bytes[32..40].try_into().unwrap()),
                        ),
                        "k {k}, {rows} rows, seed {seed}"
                    );
                    assert_eq!(
                        open_bytes(held),
                        sketch.memory_bytes() as u64,
                        "open, k {k}, {rows} rows, seed {seed}"
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
        assert_eq!(kll_open_bytes(132, rows(15)), 248 + 8 * 62 + 8 * 16);
    }
}
