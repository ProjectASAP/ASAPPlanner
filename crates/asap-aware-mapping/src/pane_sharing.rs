//! Costed reuse of compatible physical pane producers. The executor supplies
//! an equality key covering source, state, phase and evidence. This pass never
//! changes logical readout windows or assumes compatibility from metric names.

/// A concrete mergeable-pane implementation and its horizon costs.
#[derive(Debug, Clone)]
pub struct PaneReuseCandidate<K> {
    pub compatibility: K,
    pub lookback_ms: u64,
    /// Build, update, residency and retirement for this producer. Candidates
    /// with the same key must use the same unit costs and pane width, making
    /// the longest-lived producer sufficient for every readout in the group.
    pub producer_cost: f64,
    /// Readout cost for all consumers of this distinct producer.
    pub read_cost: f64,
}

#[derive(Debug, PartialEq)]
pub struct SharedPaneGroup {
    pub members: Vec<usize>,
    pub lookback_ms: u64,
    pub cost: f64,
}

/// Select build-once reuse when it is cheaper than independent producers.
/// Nonfinite quotes are ineligible, not zero-cost alternatives. The caller
/// retains independent implementations for candidates absent from the result.
pub fn select_shared_panes<K: Eq>(candidates: &[PaneReuseCandidate<K>]) -> Vec<SharedPaneGroup> {
    let mut groups: Vec<Vec<usize>> = Vec::new();
    for (index, candidate) in candidates.iter().enumerate() {
        if candidate.lookback_ms == 0
            || ![candidate.producer_cost, candidate.read_cost]
                .into_iter()
                .all(|cost| cost.is_finite() && cost >= 0.0)
        {
            continue;
        }
        if let Some(group) = groups
            .iter_mut()
            .find(|group| candidates[group[0]].compatibility == candidate.compatibility)
        {
            group.push(index);
        } else {
            groups.push(vec![index]);
        }
    }
    groups
        .into_iter()
        .filter_map(|members| {
            if members.len() < 2 {
                return None;
            }
            let mut independent = 0.0;
            let mut producer = 0.0_f64;
            let mut reads = 0.0;
            let mut lookback_ms = 0;
            for &index in &members {
                let c = &candidates[index];
                independent += c.producer_cost + c.read_cost;
                producer = producer.max(c.producer_cost);
                reads += c.read_cost;
                lookback_ms = lookback_ms.max(c.lookback_ms);
            }
            let cost = producer + reads;
            (independent.is_finite() && cost.is_finite() && cost < independent).then_some(
                SharedPaneGroup {
                    members,
                    lookback_ms,
                    cost,
                },
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    fn offer(key: &str, lookback_ms: u64, producer_cost: f64) -> PaneReuseCandidate<&str> {
        PaneReuseCandidate {
            compatibility: key,
            lookback_ms,
            producer_cost,
            read_cost: 2.0,
        }
    }
    // Share source work once while retaining both readout charges and longest history.
    #[test]
    fn shares_compatible_windows() {
        assert_eq!(
            select_shared_panes(&[
                offer("a", 60_000, 10.0),
                offer("a", 600_000, 15.0),
                offer("b", 600_000, 15.0)
            ]),
            vec![SharedPaneGroup {
                members: vec![0, 1],
                lookback_ms: 600_000,
                cost: 19.0
            }]
        );
    }
    // Invalid evidence and free producers must not fabricate a sharing benefit.
    #[test]
    fn excludes_invalid_and_nonbeneficial_quotes() {
        for cost in [0.0, f64::NAN, f64::INFINITY, -1.0, f64::MAX] {
            assert!(
                select_shared_panes(&[offer("a", 60_000, cost), offer("a", 600_000, cost)])
                    .is_empty()
            );
        }
    }
}
