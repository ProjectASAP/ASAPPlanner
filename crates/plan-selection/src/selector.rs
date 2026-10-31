//! Choosing among whole checked plans: [`PlanSelector`] and
//! [`select_among_plans`]. Today's engine evaluates each plan it proposes
//! with it, and the reference pricer (feature `reference-pricer`) selects
//! among enumerated plans with it.

use std::collections::BTreeMap;

use asap_cost::{
    worst_latency_ms, CalibrationFit, ClusterLimits, CostModel, PlanCost, PlanRanking,
    ResourceUsage, COST_UNIT,
};
use asap_types::ir::properties::AccuracyTarget;
use asap_types::physical::PhysicalWorkloadPlan;
use asap_types::workload::{DataWorkload, QueryRecurrence, QueryWorkloadEntry, RangeEvaluation};

use crate::checks::{accuracy_check, accuracy_targets, latency_violation};
use crate::report::{
    AcceptedAccuracy, ErrorSelectingPlan, Rejection, RejectionCause, WholePlanSelection,
};
use crate::SelectionModels;
use asap_cost::{kept_pane_chains, node_map, pane_retention_ms, plan_cost, query_reach};

type Ranking = PlanRanking;
use asap_types::workload::bounds::with_confirmed_weights;

/// A cost per second for a rejection reason: three decimals, or three
/// significant digits in scientific notation when it is nonzero and below
/// 0.001 (e.g. "2.361e-8" under [`ResourceUnitCost::aws_fargate_us_east_1`]'s
/// dollars), so small costs do not all read "0.000".
fn display_cost(cost_per_second: f64) -> String {
    if cost_per_second != 0.0 && cost_per_second.abs() < 1e-3 {
        format!("{cost_per_second:.3e}")
    } else {
        format!("{cost_per_second:.3}")
    }
}

/// Select among whole plans: reject candidates that the executor cannot
/// run or that miss a query's accuracy target or latency bound (or, under
/// cost model 3, the cluster's capacity), measure the rest's resource
/// usage per second and choose one under the deployment's cost model
/// (`models.cost_inputs`): the cheapest under cost models 1 and 2, the
/// best by the cluster objective under cost model 3 (the first on ties). `entries[i]` is the workload
/// entry of `candidate.roots[i]`: its accuracy target, its latency bound and
/// how often it is evaluated. Every candidate is kept in the selection
/// ([`PlanSelector`] with `keep_every_candidate`).
#[cfg(any(test, feature = "reference-pricer"))]
pub(crate) fn select_among_plans(
    cands: &[PhysicalWorkloadPlan],
    entries: &[QueryWorkloadEntry],
    data: &DataWorkload,
    models: SelectionModels<'_>,
) -> Result<WholePlanSelection, ErrorSelectingPlan> {
    let mut selector = PlanSelector::new(entries, data, models, true)?;
    if selector.needs_calibration() {
        let mut fit = CalibrationFit::default();
        for candidate in cands {
            fit.add(selector.calibration_fit(candidate)?);
        }
        selector.set_calibration(fit);
    }
    for candidate in cands {
        selector.offer(candidate)?;
    }
    selector.finish().map(|(selection, _)| selection)
}

/// Whole plans offered one at a time, in candidate order
/// ([`select_among_plans`] over all of them at once). Each candidate is
/// checked and priced as it is offered; only the best so far is kept,
/// unless `keep_every_candidate`. Either way it selects what
/// [`select_among_plans`] selects over the same candidates.
pub struct PlanSelector<'a> {
    entries: &'a [QueryWorkloadEntry],
    data: &'a DataWorkload,
    models: SelectionModels<'a>,
    targets: Vec<AccuracyTarget>,
    recurrence: Vec<QueryRecurrence>,
    range_evaluations: Vec<Option<RangeEvaluation>>,
    ranking: Ranking,
    keep_every_candidate: bool,
    candidates_checked: u64,
    costs: BTreeMap<String, PlanCost>,
    accepted: BTreeMap<String, Vec<AcceptedAccuracy>>,
    rejected: Vec<Rejection>,
    /// The valid candidates kept, in candidate order: every one, the best
    /// so far, or the Pareto front so far.
    kept: Vec<PhysicalWorkloadPlan>,
    /// The factor that calibrates the formulas' CPU time to the measured
    /// table's ([`Self::set_calibration`]); 1 without a measured table.
    pub(crate) formula_scale: f64,
}

impl<'a> PlanSelector<'a> {
    /// A selector for the workload `entries` over `data` under `models`.
    /// Bad settings are an error here, before any candidate.
    pub fn new(
        entries: &'a [QueryWorkloadEntry],
        data: &'a DataWorkload,
        models: SelectionModels<'a>,
        keep_every_candidate: bool,
    ) -> Result<Self, ErrorSelectingPlan> {
        models
            .cost_inputs
            .validate()
            .map_err(ErrorSelectingPlan::InvalidSettings)?;
        Ok(Self {
            entries,
            data,
            models,
            targets: accuracy_targets(entries),
            recurrence: entries.iter().map(|e| e.recurrence.clone()).collect(),
            range_evaluations: entries.iter().map(|e| e.range_evaluation).collect(),
            ranking: Ranking::of(&models.cost_inputs.cost_model),
            keep_every_candidate,
            candidates_checked: 0,
            costs: BTreeMap::new(),
            accepted: BTreeMap::new(),
            rejected: Vec::new(),
            kept: Vec::new(),
            formula_scale: models.cost_inputs.frozen_calibration_factor.unwrap_or(1.0),
        })
    }

    /// Whether the deployment's measured cost table prices some operator,
    /// so the formulas' CPU time is calibrated to it: one factor for every
    /// candidate, fitted over the nodes the table prices in every candidate
    /// that passes the checks before pricing. The caller then gives every
    /// candidate to [`Self::calibration_fit`], adds up the fits and passes
    /// the sum to [`Self::set_calibration`] before the first
    /// [`Self::offer`].
    pub fn needs_calibration(&self) -> bool {
        self.models.cost_inputs.frozen_calibration_factor.is_none()
            && !self
                .models
                .cost_inputs
                .measured_operation_costs
                .operators
                .is_empty()
    }

    /// `candidate`'s part of the calibration fit: its measured nodes' CPU
    /// time, as measured and as the formulas price it, when it passes the
    /// checks before pricing; nothing otherwise.
    pub fn calibration_fit(
        &self,
        candidate: &PhysicalWorkloadPlan,
    ) -> Result<CalibrationFit, ErrorSelectingPlan> {
        if candidate.roots.len() != self.entries.len() {
            return Err(ErrorSelectingPlan::EntryCount {
                candidate: candidate.id.clone(),
                roots: candidate.roots.len(),
                entries: self.entries.len(),
            });
        }
        if accuracy_check(candidate, &self.targets, self.data, &self.models).is_err() {
            return Ok(CalibrationFit::default());
        }
        let nodes = node_map(&candidate.dag);
        let kept_chains = kept_pane_chains(&candidate.dag);
        let (_, fit) = plan_cost(
            &candidate.dag,
            &nodes,
            &kept_chains,
            &self.recurrence,
            &self.range_evaluations,
            self.data,
            self.models.cost_inputs,
            1.0,
        )
        .map_err(|(node, error)| ErrorSelectingPlan::Cost {
            candidate: candidate.id.clone(),
            node,
            error,
        })?;
        Ok(fit)
    }

    /// Calibrate the formulas with `fit`, the sum of every candidate's
    /// [`Self::calibration_fit`].
    pub fn set_calibration(&mut self, fit: CalibrationFit) {
        self.formula_scale = self
            .models
            .cost_inputs
            .frozen_calibration_factor
            .unwrap_or_else(|| fit.factor());
    }

    /// Check and price `candidate`, the next in candidate order, and keep
    /// it if it can still be selected.
    #[cfg(any(test, feature = "reference-pricer"))]
    pub fn offer(&mut self, candidate: &PhysicalWorkloadPlan) -> Result<(), ErrorSelectingPlan> {
        self.offer_before(candidate, None)
    }

    /// A stopped compiler is not evidence that the candidate is invalid.
    pub fn offer_before(
        &mut self,
        candidate: &PhysicalWorkloadPlan,
        deadline: Option<std::time::Instant>,
    ) -> Result<(), ErrorSelectingPlan> {
        let check_deadline = || {
            if deadline.is_some_and(|limit| std::time::Instant::now() >= limit) {
                Err(ErrorSelectingPlan::EvaluationDeadlineExceeded)
            } else {
                Ok(())
            }
        };
        check_deadline()?;
        if candidate.roots.len() != self.entries.len() {
            return Err(ErrorSelectingPlan::EntryCount {
                candidate: candidate.id.clone(),
                roots: candidate.roots.len(),
                entries: self.entries.len(),
            });
        }
        self.candidates_checked += 1;
        let accepted_accuracy =
            match accuracy_check(candidate, &self.targets, self.data, &self.models) {
                Ok(accepted) => accepted,
                Err(failed) => {
                    self.reject(&candidate.id, failed.cause, failed.reason);
                    return Ok(());
                }
            };
        let nodes = node_map(&candidate.dag);
        let kept_chains = kept_pane_chains(&candidate.dag);
        let settings = self.models.cost_inputs;
        let (cost, _) = plan_cost(
            &candidate.dag,
            &nodes,
            &kept_chains,
            &self.recurrence,
            &self.range_evaluations,
            self.data,
            settings,
            self.formula_scale,
        )
        .map_err(|(node, error)| ErrorSelectingPlan::Cost {
            candidate: candidate.id.clone(),
            node,
            error,
        })?;
        let violation =
            latency_violation(&cost, self.entries).or_else(|| match &settings.cost_model {
                CostModel::ClusterCapacity(limits) => capacity_violation(&cost.usage, limits)
                    .map(|reason| (RejectionCause::OverClusterCapacity, reason)),
                _ => None,
            });
        if let Some((cause, reason)) = violation {
            self.reject(&candidate.id, cause, reason);
            return Ok(());
        }
        if !self.keep_every_candidate {
            // Ties keep the earlier candidate.
            let selectable = self
                .kept
                .first()
                .is_none_or(|best| self.ranking.better(&cost, &self.costs[&best.id]));
            if !selectable {
                return Ok(());
            }
        }
        check_deadline()?;
        if !self.keep_every_candidate {
            for kept in self.kept.drain(..) {
                self.costs.remove(&kept.id);
                self.accepted.remove(&kept.id);
            }
        }
        self.costs.insert(candidate.id.clone(), cost);
        self.accepted
            .insert(candidate.id.clone(), accepted_accuracy);
        self.kept.push(candidate.clone());
        Ok(())
    }

    /// The selection over every candidate offered, and the selected
    /// candidate as Stage 2 built it (the selection's plan is its roots
    /// with the weight requirements Stage 3 confirmed made proofs).
    pub fn finish(
        mut self,
    ) -> Result<(WholePlanSelection, PhysicalWorkloadPlan), ErrorSelectingPlan> {
        let ranking = self.ranking;
        let valid_costs: Vec<&PlanCost> =
            self.kept.iter().map(|kept| &self.costs[&kept.id]).collect();
        let Some(best) = choose(ranking, &valid_costs) else {
            return Err(ErrorSelectingPlan::NoValidCandidate(self.rejected));
        };
        let selected = self.kept[best].id.clone();
        for kept in &self.kept {
            let id = &kept.id;
            if *id != selected {
                let (cause, reason) = lost_reason(ranking, id, &selected, &self.costs);
                self.rejected.push(Rejection {
                    id: id.clone(),
                    cause,
                    reason,
                });
            }
        }
        let built = self.kept.swap_remove(best);
        let plan = with_confirmed_weights(&built.roots, self.data);
        let pane_retention_ms = pane_retention_ms(
            &built.dag,
            &query_reach(&built.dag, &node_map(&built.dag)),
            &self.range_evaluations,
        );
        let selection = WholePlanSelection {
            selected,
            plan,
            costs: self.costs,
            accepted: self.accepted,
            rejected: self.rejected,
            pane_retention_ms,
            candidates_checked: self.candidates_checked,
        };
        Ok((selection, built))
    }

    /// Record that candidate `id` failed a check: every rejection when
    /// keeping every candidate, otherwise the first of each cause.
    fn reject(&mut self, id: &str, cause: RejectionCause, reason: String) {
        if self.keep_every_candidate || !self.rejected.iter().any(|r| r.cause == cause) {
            self.rejected.push(Rejection {
                id: id.to_string(),
                cause,
                reason,
            });
        }
    }
}

/// The index of the candidate `ranking` selects among `costs` (the first on
/// ties); `None` without candidates.
pub(crate) fn choose(ranking: Ranking, costs: &[&PlanCost]) -> Option<usize> {
    (0..costs.len()).reduce(
        |best, next| match ranking.better(costs[next], costs[best]) {
            true => next,
            false => best,
        },
    )
}

/// Under cost model 3, the first limit of the cluster `usage` is over, as a
/// rejection reason.
fn capacity_violation(usage: &ResourceUsage, limits: &ClusterLimits) -> Option<String> {
    if usage.peak_memory_bytes > limits.memory_bytes {
        return Some(format!(
            "peak memory {} bytes is over the cluster's {} bytes",
            usage.peak_memory_bytes, limits.memory_bytes
        ));
    }
    if usage.disk_bytes > limits.disk_bytes {
        return Some(format!(
            "disk {} bytes is over the cluster's {} bytes",
            usage.disk_bytes, limits.disk_bytes
        ));
    }
    match limits.cpu_cores {
        Some(cores) if usage.peak_cpu_cores > cores => Some(format!(
            "{} CPU cores at peak is over the cluster's {cores}",
            usage.peak_cpu_cores
        )),
        _ => None,
    }
}

/// Why valid candidate `id` was not `selected` under `ranking`, and the
/// reason with its numbers.
fn lost_reason(
    ranking: Ranking,
    id: &str,
    selected: &str,
    costs: &BTreeMap<String, PlanCost>,
) -> (RejectionCause, String) {
    let (cost, best) = (&costs[id], &costs[selected]);
    if !ranking.better(best, cost) {
        let tied = match ranking {
            Ranking::Price => format!(
                "{} {COST_UNIT}",
                display_cost(cost.cost_per_second.unwrap_or_default())
            ),
            _ => describe_usage(&cost.usage),
        };
        return (
            RejectionCause::TiedLostOnEnumerationOrder,
            format!(
                "tied with the selected candidate {selected} at {tied}; lost on enumeration order"
            ),
        );
    }
    match ranking {
        Ranking::Price => (
            RejectionCause::Costlier,
            format!(
                "costlier: {} vs {} {COST_UNIT}",
                display_cost(cost.cost_per_second.unwrap_or_default()),
                display_cost(best.cost_per_second.unwrap_or_default())
            ),
        ),
        Ranking::Latency => (
            RejectionCause::Slower,
            format!(
                "slower: {} vs {}",
                describe_usage(&cost.usage),
                describe_usage(&best.usage)
            ),
        ),
        Ranking::Memory => (
            RejectionCause::MoreMemory,
            format!(
                "more memory: {} vs {}",
                describe_usage(&cost.usage),
                describe_usage(&best.usage)
            ),
        ),
    }
}

/// E.g. `latency 126.0 ms, peak memory 9648000000 bytes, disk 480000000 bytes`.
fn describe_usage(usage: &ResourceUsage) -> String {
    format!(
        "latency {:.1} ms, peak memory {} bytes, disk {} bytes",
        worst_latency_ms(usage),
        usage.peak_memory_bytes,
        usage.disk_bytes
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use asap_cost::{QueryLatency, COST_SOURCE};

    /// A candidate with the given latency, peak memory and disk under cost
    /// model 3.
    fn cluster_cost(latency_ms: f64, peak_memory_bytes: u64, disk_bytes: u64) -> PlanCost {
        PlanCost {
            usage: ResourceUsage {
                peak_memory_bytes,
                disk_bytes,
                latency_per_query: vec![QueryLatency {
                    compaction_ms: 0.0,
                    query_ms: latency_ms,
                }],
                ..ResourceUsage::default()
            },
            cost_per_second: None,
            cost_unit: COST_UNIT,
            source: COST_SOURCE,
            per_node: BTreeMap::new(),
        }
    }

    /// Peak memory and disk are compared exactly: 100 bytes less at 124 GB
    /// is less memory, not a tie.
    #[test]
    fn memory_differing_by_a_few_bytes_is_compared_exactly() {
        let larger = cluster_cost(7.0, 124_000_000_100, 0);
        let smaller = cluster_cost(7.0, 124_000_000_000, 0);
        let costs = [&larger, &smaller];
        assert_eq!(choose(Ranking::Memory, &costs).unwrap(), 1);
        assert_eq!(choose(Ranking::Latency, &costs).unwrap(), 1);
    }
}
