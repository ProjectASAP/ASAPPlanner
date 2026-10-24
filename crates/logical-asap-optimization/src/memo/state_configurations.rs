//! Finite state configurations shared by memo rules and candidate selection.
use crate::accuracy::estimators::size_for_all_readers;
use asap_types::ir::schema::{sketch::SketchKind, FieldDataType};
use asap_types::workload::greatest_common_divisor;
use std::collections::BTreeSet;

#[derive(Debug, Clone)]
pub struct PaneRequirement {
    pub pane_width_ms: u64,
    pub window_ms: u64,
    pub family: FieldDataType,
}
#[derive(Debug, Clone, PartialEq)]
pub struct PaneConfiguration {
    pub pane_width_ms: u64,
    pub longest_window_ms: u64,
    pub family: FieldDataType,
}
impl PaneConfiguration {
    pub fn serves(&self, reader: &PaneRequirement) -> bool {
        self.pane_width_ms > 0
            && reader.pane_width_ms.is_multiple_of(self.pane_width_ms)
            && reader.window_ms <= self.longest_window_ms
            && shared_size(&self.family, &reader.family)
                .is_some_and(|joined| same_family(&joined, &self.family))
    }
}
/// Existing pane-rule closure: gcd grids, distinct horizons, and joined sizes.
/// This enumerates configurations of one compatible producer, not workload plans.
pub fn pane_configurations(readers: &[PaneRequirement]) -> Vec<PaneConfiguration> {
    if readers
        .iter()
        .any(|r| r.pane_width_ms == 0 || r.window_ms == 0)
    {
        return Vec::new();
    }
    let mut grids: BTreeSet<u64> = readers.iter().map(|r| r.pane_width_ms).collect();
    loop {
        let more: Vec<u64> = grids
            .iter()
            .flat_map(|&a| grids.iter().map(move |&b| greatest_common_divisor(a, b)))
            .filter(|w| !grids.contains(w))
            .collect();
        if more.is_empty() {
            break;
        }
        grids.extend(more);
    }
    let mut thresholds = Vec::new();
    for &pane_width_ms in &grids {
        let eligible: Vec<&PaneRequirement> = readers
            .iter()
            .filter(|r| r.pane_width_ms.is_multiple_of(pane_width_ms))
            .collect();
        let windows: BTreeSet<u64> = eligible.iter().map(|r| r.window_ms).collect();
        // The sizes: the readers' own, closed under the shared size.
        let mut sizes: Vec<FieldDataType> = Vec::new();
        for reader in &eligible {
            if !sizes.iter().any(|known| same_family(known, &reader.family)) {
                sizes.push(reader.family.clone());
            }
        }
        let mut i = 0;
        while i < sizes.len() {
            for j in 0..i {
                if let Some(joined) = shared_size(&sizes[i], &sizes[j]) {
                    if !sizes.iter().any(|known| same_family(known, &joined)) {
                        sizes.push(joined);
                    }
                }
            }
            i += 1;
        }
        for &longest_window_ms in &windows {
            for family in &sizes {
                let threshold = PaneConfiguration {
                    pane_width_ms,
                    longest_window_ms,
                    family: family.clone(),
                };
                let admitted: Vec<&&PaneRequirement> =
                    eligible.iter().filter(|r| threshold.serves(r)).collect();
                let window = admitted.iter().map(|r| r.window_ms).max();
                let size = admitted.iter().try_fold(None::<FieldDataType>, |acc, r| {
                    Some(Some(match acc {
                        None => r.family.clone(),
                        Some(acc) => shared_size(&acc, &r.family)?,
                    }))
                });
                if window == Some(longest_window_ms)
                    && size
                        .flatten()
                        .is_some_and(|size| same_family(&size, family))
                {
                    thresholds.push(threshold);
                }
            }
        }
    }
    thresholds
}

pub fn shared_size(a: &FieldDataType, b: &FieldDataType) -> Option<FieldDataType> {
    match (a, b) {
        (FieldDataType::Sketch(a, grouping), FieldDataType::Sketch(b, grouping_b))
            if a.algorithm() == b.algorithm() && grouping == grouping_b =>
        {
            let params = size_for_all_readers(&[a.params(), b.params()])?;
            Some(FieldDataType::Sketch(
                SketchKind::new(a.algorithm().clone(), params),
                grouping.clone(),
            ))
        }
        (FieldDataType::Sketch(..), _) | (_, FieldDataType::Sketch(..)) => None,
        _ => (a == b).then(|| a.clone()),
    }
}

// SketchParams equality deliberately omits recorded error/probability metadata.
// Producer contracts must retain those parameters when joining and deduplicating.
fn same_family(left: &FieldDataType, right: &FieldDataType) -> bool {
    left == right && serde_json::to_string(left).ok() == serde_json::to_string(right).ok()
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

    // Equal physical capacities still carry distinct recorded error contracts;
    // only the joined contract may serve the stricter probability requirement.
    #[test]
    fn pane_configuration_keeps_failure_probability_contract() {
        let weaker = PaneConfiguration {
            pane_width_ms: 1000,
            longest_window_ms: 5000,
            family: kll(None),
        };
        let strict = PaneRequirement {
            pane_width_ms: 1000,
            window_ms: 5000,
            family: kll(Some(0.001)),
        };
        assert!(!weaker.serves(&strict));
        let configurations = pane_configurations(&[
            PaneRequirement {
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
            .all(|configuration| configuration.pane_width_ms == 1000));
    }

    // Public finite enumeration rejects invalid grids instead of dividing by zero.
    #[test]
    fn invalid_pane_grid_has_no_configurations() {
        assert!(pane_configurations(&[PaneRequirement {
            pane_width_ms: 0,
            window_ms: 5000,
            family: kll(None)
        }])
        .is_empty());
    }
}
