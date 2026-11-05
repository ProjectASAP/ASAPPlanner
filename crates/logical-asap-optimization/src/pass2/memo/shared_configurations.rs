//! Finite state configurations shared by memo rules and candidate selection.
use asap_accuracy::estimators::size_for_all_readers;
use asap_types::ir::schema::{
    sketch::{SketchKind, SketchParams},
    FieldDataType,
};
use asap_types::stages::hash_consing::same_value;
use asap_types::workload::greatest_common_divisor;
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

#[derive(Debug, Clone)]
pub(crate) struct TumblingWindowRequirement {
    pub tumbling_window_width_ms: u64,
    pub window_ms: u64,
    pub family: FieldDataType,
}
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct TumblingWindowConfiguration {
    pub tumbling_window_width_ms: u64,
    pub longest_window_ms: u64,
    pub family: FieldDataType,
}
impl TumblingWindowConfiguration {
    pub fn serves(&self, reader: &TumblingWindowRequirement) -> bool {
        self.tumbling_window_width_ms > 0
            && reader
                .tumbling_window_width_ms
                .is_multiple_of(self.tumbling_window_width_ms)
            && reader
                .window_ms
                .is_multiple_of(self.tumbling_window_width_ms)
            && self
                .longest_window_ms
                .is_multiple_of(self.tumbling_window_width_ms)
            && reader.window_ms <= self.longest_window_ms
            && shared_size(&self.family, &reader.family)
                .is_some_and(|joined| same_value(&joined, &self.family))
    }
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

/// Complete divisor grids, distinct required horizons, and joined state sizes.
/// This enumerates configurations of one compatible producer, not workload plans.
pub(crate) fn tumbling_window_configurations(
    readers: &[TumblingWindowRequirement],
) -> Vec<TumblingWindowConfiguration> {
    if readers
        .iter()
        .any(|r| r.tumbling_window_width_ms == 0 || r.window_ms == 0)
    {
        return Vec::new();
    }
    let grids =
        tumbling_window_width_domain(readers.iter().map(|reader| {
            greatest_common_divisor(reader.tumbling_window_width_ms, reader.window_ms)
        }));
    let mut configurations = Vec::new();
    for &tumbling_window_width_ms in &grids {
        let eligible: Vec<&TumblingWindowRequirement> = readers
            .iter()
            .filter(|r| {
                r.tumbling_window_width_ms
                    .is_multiple_of(tumbling_window_width_ms)
                    && r.window_ms.is_multiple_of(tumbling_window_width_ms)
            })
            .collect();
        let windows: BTreeSet<u64> = eligible.iter().map(|r| r.window_ms).collect();
        // The sizes: the readers' own, closed under the shared size.
        let mut sizes: Vec<FieldDataType> = Vec::new();
        for reader in &eligible {
            if !sizes.iter().any(|known| same_value(known, &reader.family)) {
                sizes.push(reader.family.clone());
            }
        }
        let mut i = 0;
        while i < sizes.len() {
            for j in 0..i {
                if let Some(joined) = shared_size(&sizes[i], &sizes[j]) {
                    if !sizes.iter().any(|known| same_value(known, &joined)) {
                        sizes.push(joined);
                    }
                }
            }
            i += 1;
        }
        for &longest_window_ms in &windows {
            for family in &sizes {
                let configuration = TumblingWindowConfiguration {
                    tumbling_window_width_ms,
                    longest_window_ms,
                    family: family.clone(),
                };
                let admitted: Vec<&&TumblingWindowRequirement> = eligible
                    .iter()
                    .filter(|r| configuration.serves(r))
                    .collect();
                let window = admitted.iter().map(|r| r.window_ms).max();
                let size = admitted.iter().try_fold(None::<FieldDataType>, |acc, r| {
                    Some(Some(match acc {
                        None => r.family.clone(),
                        Some(acc) => shared_size(&acc, &r.family)?,
                    }))
                });
                if window == Some(longest_window_ms)
                    && size.flatten().is_some_and(|size| same_value(&size, family))
                {
                    configurations.push(configuration);
                }
            }
        }
    }
    configurations
}

/// All distinct joined reader requirements, retaining recorded error contracts.
/// This is configuration generation only; sharing is represented by state groups.
pub(crate) fn state_size_configurations(
    requirements: Vec<Option<SketchParams>>,
) -> Vec<Option<SketchParams>> {
    if requirements.iter().any(Option::is_none) {
        return vec![None];
    }
    let requirements: Vec<SketchParams> = requirements.into_iter().flatten().collect();
    let mut joins: Vec<SketchParams> = Vec::new();
    for requirement in &requirements {
        let mut grown = vec![requirement.clone()];
        for join in &joins {
            if let Some(joined) = size_for_all_readers(&[join, requirement]) {
                grown.push(joined);
            }
        }
        for join in grown {
            if !joins.iter().any(|known| same_size(known, &join)) {
                joins.push(join);
            }
        }
    }
    let mut configurations = Vec::new();
    for configuration in joins {
        let admitted: Vec<_> = requirements
            .iter()
            .filter(|required| {
                size_for_all_readers(&[&configuration, required])
                    .is_some_and(|joined| same_size(&joined, &configuration))
            })
            .collect();
        if size_for_all_readers(&admitted).is_some_and(|joined| same_size(&joined, &configuration))
        {
            configurations.push(Some(configuration));
        }
    }
    configurations
}

fn same_size(a: &SketchParams, b: &SketchParams) -> bool {
    a == b && format!("{a:?}") == format!("{b:?}")
}

pub(crate) fn shared_size(a: &FieldDataType, b: &FieldDataType) -> Option<FieldDataType> {
    match (a, b) {
        (FieldDataType::Sketch(a, grouping), FieldDataType::Sketch(b, grouping_b))
            if a.algorithm() == b.algorithm() && grouping == grouping_b =>
        {
            let params = size_for_all_readers(&[a.params(), b.params()])?;
            Some(FieldDataType::Sketch(
                SketchKind::new(*a.algorithm(), params),
                grouping.clone(),
            ))
        }
        (FieldDataType::Sketch(..), _) | (_, FieldDataType::Sketch(..)) => None,
        _ => (a == b).then(|| a.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use asap_types::ir::schema::sketch::{GroupingStrategy, SketchAlgorithm, SketchParams};

    fn kll(probability: Option<f64>) -> FieldDataType {
        FieldDataType::Sketch(
            SketchKind::new(
                SketchAlgorithm::Kll,
                SketchParams::Kll {
                    k: 331,
                    failure_probability: probability,
                },
            ),
            GroupingStrategy::default(),
        )
    }

    // State-size generation retains both local and shared capacities without
    // depending on the removed implicit coupling-site representation.
    #[test]
    fn state_sizes_keep_each_distinct_reader_configuration() {
        let small = SketchParams::Kll {
            k: 100,
            failure_probability: None,
        };
        let large = SketchParams::Kll {
            k: 200,
            failure_probability: None,
        };
        let configurations = state_size_configurations(vec![
            Some(small.clone()),
            Some(large.clone()),
            Some(small.clone()),
        ]);
        assert_eq!(configurations.len(), 2);
        assert!(configurations.contains(&Some(small)));
        assert!(configurations.contains(&Some(large)));
        assert_eq!(state_size_configurations(vec![None, None]), vec![None]);
    }

    // Equal capacity does not erase the stricter recorded failure probability.
    #[test]
    fn state_size_join_preserves_probability_metadata() {
        let weak = SketchParams::Kll {
            k: 331,
            failure_probability: Some(0.01),
        };
        let strict = SketchParams::Kll {
            k: 331,
            failure_probability: Some(0.001),
        };
        let configurations = state_size_configurations(vec![Some(weak), Some(strict.clone())]);
        assert!(configurations
            .iter()
            .flatten()
            .any(|value| same_size(value, &strict)));
    }

    // Equal physical capacities still carry distinct recorded error contracts;
    // only the joined contract may serve the stricter probability requirement.
    #[test]
    fn tumbling_window_configuration_keeps_failure_probability_contract() {
        let weaker = TumblingWindowConfiguration {
            tumbling_window_width_ms: 1000,
            longest_window_ms: 5000,
            family: kll(None),
        };
        let strict = TumblingWindowRequirement {
            tumbling_window_width_ms: 1000,
            window_ms: 5000,
            family: kll(Some(0.001)),
        };
        assert!(!weaker.serves(&strict));
        let configurations = tumbling_window_configurations(&[
            TumblingWindowRequirement {
                family: kll(None),
                ..strict.clone()
            },
            strict.clone(),
        ]);
        assert!(configurations
            .iter()
            .any(|configuration| configuration.serves(&strict)));
        assert!(configurations
            .iter()
            .all(|configuration| 1000_u64.is_multiple_of(configuration.tumbling_window_width_ms)));
    }

    // Every integer-millisecond width that divides the grid is offered, not
    // just the coarsest width or GCDs of the readers' original configurations.
    #[test]
    fn tumbling_window_width_domain_contains_every_legal_divisor() {
        let reader = TumblingWindowRequirement {
            tumbling_window_width_ms: 12,
            window_ms: 60,
            family: kll(None),
        };
        let widths: BTreeSet<_> = tumbling_window_configurations(&[reader])
            .into_iter()
            .map(|configuration| configuration.tumbling_window_width_ms)
            .collect();
        assert_eq!(widths, BTreeSet::from([1, 2, 3, 4, 6, 12]));
    }

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

    // Public finite enumeration rejects invalid grids instead of dividing by zero.
    #[test]
    fn invalid_tumbling_window_grid_has_no_configurations() {
        assert!(tumbling_window_configurations(&[TumblingWindowRequirement {
            tumbling_window_width_ms: 0,
            window_ms: 5000,
            family: kll(None)
        }])
        .is_empty());
    }
}
