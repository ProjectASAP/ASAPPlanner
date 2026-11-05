//! The tumbling-window widths rule 1 offers: the divisors of the queries'
//! evaluation grids.
use std::collections::BTreeSet;

/// Every tumbling-window width that divides one of `grids` (zero grids
/// ignored), in ascending order.
pub(crate) fn tumbling_window_width_domain(grids: impl IntoIterator<Item = u64>) -> Vec<u64> {
    let grids: BTreeSet<u64> = grids.into_iter().filter(|&grid| grid > 0).collect();
    let mut widths = BTreeSet::new();
    for grid in grids {
        widths.extend(tumbling_window_width_divisors(grid));
    }
    widths.into_iter().collect()
}

/// Every positive integer-millisecond divisor of a grid, in ascending order.
/// The time representation fixes the search resolution; no representative-width
/// sampling or arbitrary candidate cap changes this domain.
pub(crate) fn tumbling_window_width_divisors(grid_ms: u64) -> Vec<u64> {
    let mut widths = BTreeSet::new();
    let mut divisor = 1;
    while divisor <= grid_ms / divisor {
        if grid_ms.is_multiple_of(divisor) {
            widths.insert(divisor);
            widths.insert(grid_ms / divisor);
        }
        divisor += 1;
    }
    widths.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // Compare the divisor generator to direct enumeration, including zero.
    #[test]
    fn width_divisors_match_complete_integer_domain() {
        for grid in 0..=256_u64 {
            let expected: Vec<_> = (1..=grid)
                .filter(|width| grid.is_multiple_of(*width))
                .collect();
            assert_eq!(tumbling_window_width_divisors(grid), expected);
        }
    }
}
