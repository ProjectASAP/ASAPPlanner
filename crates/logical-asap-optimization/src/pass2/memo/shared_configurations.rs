//! Finite state configurations shared by memo rules and candidate selection.
use asap_accuracy::estimators::size_for_all_readers;
use asap_types::ir::schema::{
    sketch::{SketchKind, SketchParams},
    FieldDataType,
};
use asap_types::workload::greatest_common_divisor;
use std::collections::BTreeSet;

/// Width domain used by the same candidate rules. Seed grids are only a
/// feasible-plan heuristic; optimality must use the complete domain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PaneWidthDomain {
    #[default]
    Complete,
    SeedGrids,
    /// Seed grids plus widths obtained by dividing each grid into at most
    /// `max_parts` equal pieces. Increasing the limit monotonically exposes
    /// every legal divisor; only `Complete` certifies full-domain search.
    Expanding {
        max_parts: u64,
    },
}

pub fn pane_width_domain(
    grids: impl IntoIterator<Item = u64>,
    domain: PaneWidthDomain,
) -> Vec<u64> {
    pane_width_domain_before(grids, domain, None)
        .expect("generation without a deadline cannot time out")
}

pub(crate) fn pane_width_domain_before(
    grids: impl IntoIterator<Item = u64>,
    domain: PaneWidthDomain,
    deadline: Option<std::time::Instant>,
) -> Result<Vec<u64>, asap_types::stages::CandidateGenerationTimeout> {
    super::check_generation_deadline(deadline)?;
    let mut widths = BTreeSet::new();
    for width in grids {
        super::check_generation_deadline(deadline)?;
        if width > 0 {
            widths.insert(width);
        }
    }
    if domain == PaneWidthDomain::Complete {
        let mut complete = BTreeSet::new();
        for grid in widths {
            complete.extend(pane_width_divisors_before(grid, deadline)?);
        }
        return Ok(complete.into_iter().collect());
    }
    let grids = widths.clone();
    loop {
        let mut additional = BTreeSet::new();
        for &a in &widths {
            for &b in &widths {
                super::check_generation_deadline(deadline)?;
                let width = greatest_common_divisor(a, b);
                if !widths.contains(&width) {
                    additional.insert(width);
                }
            }
        }
        if additional.is_empty() {
            break;
        }
        widths.extend(additional);
    }
    if let PaneWidthDomain::Expanding { max_parts } = domain {
        // Work is proportional to the requested frontier, not the complete
        // divisor domain. Do not introduce the finest reciprocal width early.
        for grid in grids {
            for parts in 2..=max_parts.min(grid) {
                if parts % 1024 == 2 {
                    super::check_generation_deadline(deadline)?;
                }
                if grid.is_multiple_of(parts) {
                    widths.insert(grid / parts);
                }
            }
        }
    }
    super::check_generation_deadline(deadline)?;
    Ok(widths.into_iter().collect())
}

/// Integer-domain frontier, independent of expensive candidate graph expansion.
/// Complete divisor lists are computed once under the caller's deadline.
pub struct PaneWidthFrontier {
    grids: Vec<u64>,
    complete: Vec<Vec<u64>>,
    previous: Vec<Vec<u64>>,
    max_parts: u64,
    finished: bool,
}

impl PaneWidthFrontier {
    pub fn new(
        grids: impl IntoIterator<Item = u64>,
        deadline: Option<std::time::Instant>,
    ) -> Result<Self, asap_types::stages::CandidateGenerationTimeout> {
        let grids: Vec<_> = grids
            .into_iter()
            .filter(|grid| *grid > 0)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let complete = grids
            .iter()
            .map(|&grid| pane_width_divisors_before(grid, deadline))
            .collect::<Result<Vec<_>, _>>()?;
        let previous = grids.iter().map(|&grid| vec![grid]).collect();
        Ok(Self {
            grids,
            complete,
            previous,
            max_parts: 4,
            finished: false,
        })
    }

    /// Skip frontiers which expose no new width for any individual reader grid.
    /// Comparing per-grid sets also protects group-specific supplier choices.
    pub fn next_domain(
        &mut self,
        deadline: Option<std::time::Instant>,
    ) -> Result<Option<PaneWidthDomain>, asap_types::stages::CandidateGenerationTimeout> {
        while !self.finished {
            super::check_generation_deadline(deadline)?;
            let current: Vec<Vec<u64>> = self
                .grids
                .iter()
                .zip(&self.complete)
                .map(|(&grid, widths)| {
                    widths
                        .iter()
                        .copied()
                        .filter(|width| grid / width <= self.max_parts)
                        .collect()
                })
                .collect();
            self.max_parts = self.max_parts.saturating_mul(2);
            if current == self.complete {
                self.finished = true;
                return Ok(Some(PaneWidthDomain::Complete));
            }
            if current != self.previous {
                self.previous = current;
                return Ok(Some(PaneWidthDomain::Expanding {
                    max_parts: self.max_parts / 2,
                }));
            }
        }
        Ok(None)
    }
}

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
            && reader.window_ms.is_multiple_of(self.pane_width_ms)
            && self.longest_window_ms.is_multiple_of(self.pane_width_ms)
            && reader.window_ms <= self.longest_window_ms
            && shared_size(&self.family, &reader.family)
                .is_some_and(|joined| same_family(&joined, &self.family))
    }
}
/// Every positive integer-millisecond divisor of a grid, in ascending order.
/// The time representation fixes the search resolution; no representative-width
/// sampling or arbitrary candidate cap changes this domain.
pub fn pane_width_divisors(grid_ms: u64) -> Vec<u64> {
    pane_width_divisors_before(grid_ms, None)
        .expect("generation without a deadline cannot time out")
}

fn pane_width_divisors_before(
    grid_ms: u64,
    deadline: Option<std::time::Instant>,
) -> Result<Vec<u64>, asap_types::stages::CandidateGenerationTimeout> {
    super::check_generation_deadline(deadline)?;
    let mut widths = BTreeSet::new();
    let mut divisor = 1;
    while divisor <= grid_ms / divisor {
        // A large user-provided grid must not monopolize the planning budget.
        if divisor % 1024 == 1 {
            super::check_generation_deadline(deadline)?;
        }
        if grid_ms.is_multiple_of(divisor) {
            widths.insert(divisor);
            widths.insert(grid_ms / divisor);
        }
        divisor += 1;
    }
    super::check_generation_deadline(deadline)?;
    Ok(widths.into_iter().collect())
}

/// Complete divisor grids, distinct required horizons, and joined state sizes.
/// This enumerates configurations of one compatible producer, not workload plans.
pub fn pane_configurations(readers: &[PaneRequirement]) -> Vec<PaneConfiguration> {
    pane_configurations_in_domain(readers, PaneWidthDomain::Complete)
}

pub fn pane_configurations_in_domain(
    readers: &[PaneRequirement],
    domain: PaneWidthDomain,
) -> Vec<PaneConfiguration> {
    pane_configurations_before(readers, domain, None)
        .expect("generation without a deadline cannot time out")
}

pub(crate) fn pane_configurations_before(
    readers: &[PaneRequirement],
    domain: PaneWidthDomain,
    deadline: Option<std::time::Instant>,
) -> Result<Vec<PaneConfiguration>, asap_types::stages::CandidateGenerationTimeout> {
    super::check_generation_deadline(deadline)?;
    if readers
        .iter()
        .any(|r| r.pane_width_ms == 0 || r.window_ms == 0)
    {
        return Ok(Vec::new());
    }
    let grids = pane_width_domain_before(
        readers
            .iter()
            .map(|reader| greatest_common_divisor(reader.pane_width_ms, reader.window_ms)),
        domain,
        deadline,
    )?;
    let mut configurations = Vec::new();
    for &pane_width_ms in &grids {
        super::check_generation_deadline(deadline)?;
        let eligible: Vec<&PaneRequirement> = readers
            .iter()
            .filter(|r| {
                r.pane_width_ms.is_multiple_of(pane_width_ms)
                    && r.window_ms.is_multiple_of(pane_width_ms)
            })
            .collect();
        let windows: BTreeSet<u64> = eligible.iter().map(|r| r.window_ms).collect();
        // The sizes: the readers' own, closed under the shared size.
        let mut sizes: Vec<FieldDataType> = Vec::new();
        for reader in &eligible {
            super::check_generation_deadline(deadline)?;
            if !sizes.iter().any(|known| same_family(known, &reader.family)) {
                sizes.push(reader.family.clone());
            }
        }
        let mut i = 0;
        while i < sizes.len() {
            super::check_generation_deadline(deadline)?;
            for j in 0..i {
                super::check_generation_deadline(deadline)?;
                if let Some(joined) = shared_size(&sizes[i], &sizes[j]) {
                    if !sizes.iter().any(|known| same_family(known, &joined)) {
                        sizes.push(joined);
                    }
                }
            }
            i += 1;
        }
        for &longest_window_ms in &windows {
            super::check_generation_deadline(deadline)?;
            for family in &sizes {
                super::check_generation_deadline(deadline)?;
                let configuration = PaneConfiguration {
                    pane_width_ms,
                    longest_window_ms,
                    family: family.clone(),
                };
                let admitted: Vec<&&PaneRequirement> = eligible
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
                    && size
                        .flatten()
                        .is_some_and(|size| same_family(&size, family))
                {
                    configurations.push(configuration);
                }
            }
        }
    }
    super::check_generation_deadline(deadline)?;
    Ok(configurations)
}

/// All distinct joined reader requirements, retaining recorded error contracts.
/// This is configuration generation only; sharing is represented by state groups.
pub(crate) fn state_size_configurations(
    requirements: Vec<Option<SketchParams>>,
    deadline: Option<std::time::Instant>,
) -> Result<Vec<Option<SketchParams>>, asap_types::stages::CandidateGenerationTimeout> {
    super::check_generation_deadline(deadline)?;
    if requirements.iter().any(Option::is_none) {
        return Ok(vec![None]);
    }
    let requirements: Vec<SketchParams> = requirements.into_iter().flatten().collect();
    let mut joins: Vec<SketchParams> = Vec::new();
    for requirement in &requirements {
        super::check_generation_deadline(deadline)?;
        let mut grown = vec![requirement.clone()];
        for join in &joins {
            super::check_generation_deadline(deadline)?;
            if let Some(joined) = size_for_all_readers(&[join, requirement]) {
                grown.push(joined);
            }
        }
        for join in grown {
            super::check_generation_deadline(deadline)?;
            if !joins.iter().any(|known| same_size(known, &join)) {
                joins.push(join);
            }
        }
    }
    let mut configurations = Vec::new();
    for configuration in joins {
        super::check_generation_deadline(deadline)?;
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
    Ok(configurations)
}

fn same_size(a: &SketchParams, b: &SketchParams) -> bool {
    a == b && format!("{a:?}") == format!("{b:?}")
}

pub fn shared_size(a: &FieldDataType, b: &FieldDataType) -> Option<FieldDataType> {
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
        let configurations = state_size_configurations(
            vec![
                Some(small.clone()),
                Some(large.clone()),
                Some(small.clone()),
            ],
            None,
        )
        .unwrap();
        assert_eq!(configurations.len(), 2);
        assert!(configurations.contains(&Some(small)));
        assert!(configurations.contains(&Some(large)));
        assert_eq!(
            state_size_configurations(vec![None, None], None).unwrap(),
            vec![None]
        );
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
        let configurations =
            state_size_configurations(vec![Some(weak), Some(strict.clone())], None).unwrap();
        assert!(configurations
            .iter()
            .flatten()
            .any(|value| same_size(value, &strict)));
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
            .all(|configuration| 1000_u64.is_multiple_of(configuration.pane_width_ms)));
    }

    // Every integer-millisecond width that divides the grid is offered, not
    // just the coarsest width or GCDs of the readers' original configurations.
    #[test]
    fn width_frontier_skips_empty_rounds_and_detects_actual_completion() {
        let mut frontier = PaneWidthFrontier::new([97], None).unwrap();
        assert_eq!(
            frontier.next_domain(None).unwrap(),
            Some(PaneWidthDomain::Complete)
        );
        assert_eq!(frontier.next_domain(None).unwrap(), None);
        let mut frontier = PaneWidthFrontier::new([12], None).unwrap();
        assert_eq!(
            frontier.next_domain(None).unwrap(),
            Some(PaneWidthDomain::Expanding { max_parts: 4 })
        );
        assert_eq!(
            frontier.next_domain(None).unwrap(),
            Some(PaneWidthDomain::Expanding { max_parts: 8 })
        );
        assert_eq!(
            frontier.next_domain(None).unwrap(),
            Some(PaneWidthDomain::Complete)
        );
        assert!(PaneWidthFrontier::new([u64::MAX], Some(std::time::Instant::now())).is_err());
    }

    #[test]
    fn expanding_widths_are_nested_and_eventually_complete() {
        let grids = [60, 90];
        let mut previous = pane_width_domain(grids, PaneWidthDomain::SeedGrids);
        for max_parts in [2, 4, 8, 16, 32, 64, 128] {
            let next = pane_width_domain(grids, PaneWidthDomain::Expanding { max_parts });
            assert!(previous.iter().all(|width| next.contains(width)));
            assert!(next
                .iter()
                .all(|width| grids.iter().any(|grid| grid % width == 0)));
            previous = next;
        }
        assert_eq!(
            previous,
            pane_width_domain(grids, PaneWidthDomain::Complete)
        );
    }

    #[test]
    fn expanding_domain_defers_fine_panes_and_obeys_deadline() {
        let widths = pane_width_domain([60_000], PaneWidthDomain::Expanding { max_parts: 4 });
        assert_eq!(widths, vec![15_000, 20_000, 30_000, 60_000]);
        assert!(pane_width_domain_before(
            [u64::MAX],
            PaneWidthDomain::Expanding {
                max_parts: u64::MAX
            },
            Some(std::time::Instant::now()),
        )
        .is_err());
    }

    #[test]
    fn pane_width_domain_contains_every_legal_divisor() {
        let reader = PaneRequirement {
            pane_width_ms: 12,
            window_ms: 60,
            family: kll(None),
        };
        let widths: BTreeSet<_> = pane_configurations(&[reader])
            .into_iter()
            .map(|configuration| configuration.pane_width_ms)
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
            assert_eq!(pane_width_divisors(grid), expected);
        }
    }

    // Cancellation is checked during divisor enumeration, not only between
    // configurations: this grid cannot be fully factored within the budget.
    #[test]
    fn large_grid_enumeration_honors_deadline() {
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(2);
        assert!(
            pane_width_domain_before([u64::MAX], PaneWidthDomain::Complete, Some(deadline),)
                .is_err()
        );
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
