//! Each group candidate priced alone, and local dominance per signature
//! (steps 2–3).
//!
//! A candidate is rebuilt by Stage 2 from its roots and materialization,
//! then checked and priced by Stage 3 with the group's queries as the whole
//! workload ([`PlanSelector::price_alone`]). What happens to a candidate
//! that fails a check:
//!
//! - a failure no larger summary repairs (`size_independent`: an
//!   unrepairable accuracy failure, a per-series aggregate the executor
//!   cannot run, raw data the deployment does not keep, a latency bound or
//!   repeat interval broken) drops it: sharing only makes a site's summary
//!   larger, which cannot repair these;
//! - a size-dependent failure (an accuracy target missed at this size, a
//!   resize that did not rebuild) keeps it unpriced (`usage: None`) when it
//!   exposes a sharing site, since sharing that site with a stricter reader
//!   may size it up; it is dropped when it exposes none, since then nothing
//!   outside the group changes its sizes.
//!
//! Prices are cached by the candidate's flattened physical DAG (which holds
//! each node's materialization) and its queries' requirements and
//! recurrence: groups of identical shape price each candidate once.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use asap_physical_asap_optimization::{optimize_physical_asap_assigned, PhysicalCandidate};
use asap_types::workload::{DataWorkload, QueryWorkloadEntry};

use super::{GroupCandidate, LocalUsage, MemoGroup};
use crate::{CandidateCost, PlanSelector, PlanningModels, PricedAlone, SelectionError};

/// The local prices of one search's group candidates, by the candidate's
/// structural key ([`price_key`]). Valid for one data workload and one set
/// of planning models.
#[derive(Debug, Default)]
pub(crate) struct LocalPriceCache {
    prices: HashMap<String, LocalPrice>,
    /// Candidates whose price was found in the cache.
    pub hits: u64,
    /// Candidates priced by Stage 3 (cache misses).
    pub misses: u64,
    /// Time spent rebuilding and pricing the missed candidates.
    pub pricing_time: Duration,
}

/// One candidate's outcome of Stage 3's checks and pricing, alone.
#[derive(Debug, Clone, PartialEq)]
enum LocalPrice {
    Priced(LocalUsage),
    /// A check failed that a larger summary may pass.
    SizeDependentFailure,
    /// A check failed that sharing cannot repair, or Stage 2 could not
    /// rebuild it.
    Unrepairable,
}

/// `candidates` of `group`, each priced by Stage 3 with the group alone
/// (`usage` filled), the ones failing a check sharing cannot repair
/// dropped and the ones failing a size-dependent check kept unpriced when
/// they expose a sharing site (module doc). `entries` is the whole
/// workload's. An error is Stage 3's: bad settings, or a node it cannot
/// price.
pub(crate) fn priced(
    group: &MemoGroup,
    candidates: Vec<GroupCandidate>,
    entries: &[QueryWorkloadEntry],
    data: &DataWorkload,
    models: PlanningModels<'_>,
    cache: &mut LocalPriceCache,
) -> Result<Vec<GroupCandidate>, SelectionError> {
    let group_entries: Vec<QueryWorkloadEntry> =
        group.queries.iter().map(|&q| entries[q].clone()).collect();
    let selector = PlanSelector::new(&group_entries, data, models, false)?;
    let mut kept = Vec::with_capacity(candidates.len());
    for (index, mut candidate) in candidates.into_iter().enumerate() {
        let started = Instant::now();
        let Some(mut physical) = rebuilt(&candidate, &group_entries, data, &models) else {
            continue;
        };
        physical.id = format!("G{}-{}", candidate.group, index + 1);
        let key = price_key(&physical, &group_entries);
        let price = match cache.prices.get(&key) {
            Some(price) => {
                cache.hits += 1;
                price.clone()
            }
            None => {
                let price = match selector.price_alone(&physical)? {
                    PricedAlone::Priced(cost) => LocalPrice::Priced(local_usage(&cost)),
                    PricedAlone::Failed(failure) if failure.size_independent => {
                        LocalPrice::Unrepairable
                    }
                    PricedAlone::Failed(_) => LocalPrice::SizeDependentFailure,
                };
                cache.misses += 1;
                cache.pricing_time += started.elapsed();
                cache.prices.insert(key, price.clone());
                price
            }
        };
        match price {
            LocalPrice::Priced(usage) => candidate.usage = Some(usage),
            LocalPrice::SizeDependentFailure if !candidate.signature.0.is_empty() => {
                candidate.usage = None
            }
            LocalPrice::SizeDependentFailure | LocalPrice::Unrepairable => continue,
        }
        kept.push(candidate);
    }
    Ok(kept)
}

/// `candidate`'s physical candidate, as Stage 2 builds it from its roots
/// with its materialization; `None` when Stage 2 does not build it with
/// exactly that materialization.
fn rebuilt(
    candidate: &GroupCandidate,
    group_entries: &[QueryWorkloadEntry],
    data: &DataWorkload,
    models: &PlanningModels<'_>,
) -> Option<PhysicalCandidate> {
    let mut materialization = candidate.materialization.iter().cloned();
    let mut choose = |_: &[usize]| materialization.next();
    let built = optimize_physical_asap_assigned(
        "G",
        &candidate.roots,
        group_entries,
        data,
        models.deployment_capabilities,
        &mut choose,
    );
    let physical = built.ok().flatten()?;
    materialization.next().is_none().then_some(physical)
}

/// The structural key of a group's physical candidate: its flattened DAG,
/// which holds each node's materialization, and what Stage 3 reads of the
/// group's entries (everything but the query text).
fn price_key(physical: &PhysicalCandidate, group_entries: &[QueryWorkloadEntry]) -> String {
    let requirements: Vec<_> = group_entries
        .iter()
        .map(|e| {
            (
                &e.requirements,
                &e.predictability,
                &e.recurrence,
                &e.range_evaluation,
            )
        })
        .collect();
    serde_json::to_string(&(&physical.dag, requirements))
        .expect("a physical DAG and workload entries serialize")
}

/// `cost`'s usage as a group candidate's local usage.
fn local_usage(cost: &CandidateCost) -> LocalUsage {
    let usage = &cost.usage;
    LocalUsage {
        cpu_ops_per_second: usage.cpu_ops_per_second,
        scanned_bytes_per_second: usage.scanned_bytes_per_second,
        average_memory_bytes: usage.average_memory_bytes,
        peak_memory_bytes: usage.peak_memory_bytes,
        disk_bytes: usage.disk_bytes,
        latency_ms: usage
            .latency_per_query
            .iter()
            .map(|latency| latency.total_ms())
            .collect(),
        cost_per_second: cost.cost_per_second,
    }
}

/// `candidates` (priced) with every one another of the same signature
/// dominates dropped: no greater on every usage component and each query's
/// latency, and smaller on one. Ties are kept, in order. An unpriced
/// candidate is kept and dominates none.
///
/// Per signature, the priced candidates are visited in lexicographic order
/// of their usage vectors, so a candidate's dominators come before it, and
/// each is compared with the undominated ones found so far: O(n log n +
/// n·front).
pub(crate) fn keep_undominated(candidates: Vec<GroupCandidate>) -> Vec<GroupCandidate> {
    let mut by_signature: HashMap<&super::SharingSignature, Vec<usize>> = HashMap::new();
    let vectors: Vec<Option<Vec<f64>>> = candidates
        .iter()
        .map(|c| c.usage.as_ref().map(usage_vector))
        .collect();
    for (index, candidate) in candidates.iter().enumerate() {
        if vectors[index].is_some() {
            by_signature
                .entry(&candidate.signature)
                .or_default()
                .push(index);
        }
    }
    let mut dominated = vec![false; candidates.len()];
    for mut indices in by_signature.into_values() {
        let vector = |i: usize| vectors[i].as_ref().expect("priced");
        indices.sort_by(|&a, &b| lexicographic(vector(a), vector(b)));
        let mut front: Vec<usize> = Vec::new();
        for index in indices {
            if front
                .iter()
                .any(|&kept| dominates(vector(kept), vector(index)))
            {
                dominated[index] = true;
            } else {
                front.push(index);
            }
        }
    }
    candidates
        .into_iter()
        .zip(dominated)
        .filter_map(|(candidate, dominated)| (!dominated).then_some(candidate))
        .collect()
}

/// The components dominance compares, in a fixed order: the price (when
/// the cost model has one), CPU, scans, average and peak memory, disk, and
/// each query's latency.
fn usage_vector(usage: &LocalUsage) -> Vec<f64> {
    let mut vector: Vec<f64> = usage.cost_per_second.into_iter().collect();
    vector.extend([
        usage.cpu_ops_per_second,
        usage.scanned_bytes_per_second,
        usage.average_memory_bytes,
        usage.peak_memory_bytes as f64,
        usage.disk_bytes as f64,
    ]);
    vector.extend(&usage.latency_ms);
    vector
}

fn lexicographic(a: &[f64], b: &[f64]) -> std::cmp::Ordering {
    a.iter()
        .zip(b)
        .map(|(x, y)| x.total_cmp(y))
        .find(|order| order.is_ne())
        .unwrap_or_else(|| a.len().cmp(&b.len()))
}

/// Whether `a` is no greater than `b` on every component and smaller on
/// one. Vectors of different shapes (one priced, one not; different query
/// counts) are incomparable.
fn dominates(a: &[f64], b: &[f64]) -> bool {
    a.len() == b.len()
        && a.iter().zip(b).all(|(x, y)| x <= y)
        && a.iter().zip(b).any(|(x, y)| x < y)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pass::memo::{SharingSignature, SharingSite};

    fn usage(cost: f64, latency_ms: f64) -> LocalUsage {
        LocalUsage {
            cpu_ops_per_second: 10.0,
            scanned_bytes_per_second: 10.0,
            average_memory_bytes: 10.0,
            peak_memory_bytes: 10,
            disk_bytes: 10,
            latency_ms: vec![latency_ms],
            cost_per_second: Some(cost),
        }
    }

    fn candidate(index: usize, signature: &[u64], usage: Option<LocalUsage>) -> GroupCandidate {
        GroupCandidate {
            group: 0,
            choice: vec![index],
            roots: Vec::new(),
            materialization: Vec::new(),
            signature: SharingSignature(
                signature
                    .iter()
                    .map(|&key| SharingSite {
                        meeting_key: key,
                        cost_key: key,
                    })
                    .collect(),
            ),
            usage,
        }
    }

    fn kept(candidates: Vec<GroupCandidate>) -> Vec<usize> {
        keep_undominated(candidates)
            .iter()
            .map(|c| c.choice[0])
            .collect()
    }

    #[test]
    fn a_dominated_candidate_is_dropped() {
        let cheap = candidate(0, &[], Some(usage(1.0, 5.0)));
        let mut costly = usage(2.0, 5.0);
        costly.peak_memory_bytes = 20;
        let costly = candidate(1, &[], Some(costly));
        assert_eq!(kept(vec![costly, cheap]), vec![0]);
    }

    #[test]
    fn a_candidate_of_another_signature_is_kept_even_if_costlier() {
        let cheap = candidate(0, &[], Some(usage(1.0, 5.0)));
        let costly = candidate(1, &[7], Some(usage(2.0, 6.0)));
        assert_eq!(kept(vec![cheap, costly]), vec![0, 1]);
    }

    #[test]
    fn ties_are_kept_in_order() {
        let candidates = vec![
            candidate(2, &[7], Some(usage(1.0, 5.0))),
            candidate(0, &[7], Some(usage(1.0, 5.0))),
            candidate(1, &[7], Some(usage(1.0, 5.0))),
        ];
        assert_eq!(kept(candidates), vec![2, 0, 1]);
    }

    #[test]
    fn a_cheaper_candidate_with_a_higher_latency_is_kept() {
        let fast = candidate(0, &[], Some(usage(2.0, 5.0)));
        let cheap = candidate(1, &[], Some(usage(1.0, 9.0)));
        assert_eq!(kept(vec![fast, cheap]), vec![0, 1]);
    }

    #[test]
    fn an_unpriced_candidate_is_kept_and_dominates_none() {
        let unpriced = candidate(0, &[7], None);
        let priced = candidate(1, &[7], Some(usage(1.0, 5.0)));
        assert_eq!(kept(vec![unpriced, priced]), vec![0, 1]);
    }

    #[test]
    fn dominance_is_found_through_a_chain() {
        // 2 dominates 1, which dominates 0: only 2 is kept, whatever the
        // order.
        let candidates = vec![
            candidate(0, &[], Some(usage(3.0, 7.0))),
            candidate(1, &[], Some(usage(2.0, 6.0))),
            candidate(2, &[], Some(usage(1.0, 5.0))),
        ];
        assert_eq!(kept(candidates), vec![2]);
    }

    /// A one-query workload's group candidate, rebuilt from the selected
    /// plan's roots and materialization, is priced as the joint search
    /// prices that plan.
    #[test]
    fn priced_alone_matches_the_selected_plans_price() {
        use asap_logical_asap_optimization::test_support::{entry_with, lower_promql};
        use asap_types::ir::properties::AccuracyTarget;
        use asap_types::ir::QueryRoot;
        use asap_types::workload::{
            DataArrival, QueryRecurrence, RepeatedDemand, RepetitionInterval, SourceFacts,
        };

        let target = AccuracyTarget::max_error(0.01).failure_probability(0.001);
        let root = lower_promql("quantile_over_time(0.99, m[5m])", target.clone());
        let entries = [entry_with(
            target,
            QueryRecurrence::Repeated(RepeatedDemand::FixedInterval(RepetitionInterval(60_000))),
        )];
        let data = DataWorkload {
            arrival: DataArrival::Ingesting {
                rows_per_second: 10_000.0,
            },
            sources: [(
                "m".to_string(),
                SourceFacts {
                    series_count: Some(10_000),
                    ..SourceFacts::default()
                },
            )]
            .into_iter()
            .collect(),
        };
        let models = PlanningModels::builtin();
        let run = crate::pass::run_stage1_to_stage3(
            vec![(0, QueryRoot::Operator(root))],
            &entries,
            &data,
            models,
        )
        .unwrap();
        let selection = run.stage3_plan_selection.as_ref().unwrap();
        let expected = selection.costs[&selection.selected].cost_per_second;
        let selected = run.stage3_selected.as_ref().unwrap();
        let logical = run.stage1_selected.as_ref().unwrap();
        let group = MemoGroup {
            queries: vec![0],
            targets: (0..logical.choice.len()).collect(),
        };
        let group_candidate = GroupCandidate {
            group: 0,
            choice: logical.choice.clone(),
            roots: logical.roots.as_ref().unwrap().clone(),
            materialization: selected.assignment.clone(),
            signature: SharingSignature::default(),
            usage: None,
        };
        let mut cache = LocalPriceCache::default();
        let twice = vec![group_candidate.clone(), group_candidate];
        let priced = priced(&group, twice, &entries, &data, models, &mut cache).unwrap();
        assert_eq!(priced.len(), 2);
        for candidate in &priced {
            assert_eq!(candidate.usage.as_ref().unwrap().cost_per_second, expected);
        }
        assert_eq!((cache.misses, cache.hits), (1, 1));
    }
}
