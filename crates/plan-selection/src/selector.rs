//! Choosing among checked candidates: [`select_plan`], [`PlanSelector`] and the
//! cost model rankings.

use std::collections::BTreeMap;

use asap_cost::{
    CalibrationFit, CandidateCost, ClusterLimits, ClusterObjective, CostModel, QueryLatency,
    ResourceUsage, COST_UNIT,
};
use asap_physical_asap_optimization::PhysicalWorkloadPlan;
use asap_types::ir::properties::AccuracyTarget;
use asap_types::workload::{DataWorkload, QueryRecurrence, QueryWorkloadEntry, RangeEvaluation};

use crate::checks::{
    accuracy_check, accuracy_targets, latency_violation, per_entity_aggregate_violation,
    raw_data_violation, CheckFailure,
};
use crate::pricing::{kept_pane_chains, node_map, pane_retention_ms, price_at_scale, query_reach};
use crate::report::{
    AcceptedAccuracy, Rejection, RejectionCause, SearchReport, Selection, SelectionError,
};
use crate::sizing::sized_for_failure_probability;
use crate::statistics::with_confirmed_weights;
use crate::PlanningModels;

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

/// Stage 3 (plan selection): reject candidates that the executor cannot
/// run or that miss a query's accuracy target or latency bound (or, under
/// cost model 3, the cluster's capacity), measure the rest's resource
/// usage per second and choose one under the deployment's cost model
/// (`models.plan_selection`): the cheapest under cost models 1 and 2, the
/// best by the cluster objective under cost model 3 (the first on ties). `entries[i]` is the workload
/// entry of `candidate.roots[i]`: its accuracy target, its latency bound and
/// how often it is evaluated. Every candidate is kept in the selection
/// ([`PlanSelector`] with `keep_every_candidate`).
pub fn select_plan(
    cands: &[PhysicalWorkloadPlan],
    entries: &[QueryWorkloadEntry],
    data: &DataWorkload,
    models: PlanningModels<'_>,
) -> Result<Selection, SelectionError> {
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

/// Stage 3 over candidates offered one at a time, in candidate order
/// ([`select_plan`] over all of them at once). Each candidate is checked
/// and priced as it is offered; only the best so far is kept (under
/// [`ClusterObjective::ParetoFront`], the Pareto front so far), unless
/// `keep_every_candidate`. Either way it selects what [`select_plan`]
/// selects over the same candidates.
pub struct PlanSelector<'a> {
    entries: &'a [QueryWorkloadEntry],
    data: &'a DataWorkload,
    models: PlanningModels<'a>,
    targets: Vec<AccuracyTarget>,
    recurrence: Vec<QueryRecurrence>,
    range_evaluations: Vec<Option<RangeEvaluation>>,
    ranking: Ranking,
    keep_every_candidate: bool,
    candidates_checked: u64,
    costs: BTreeMap<String, CandidateCost>,
    accepted: BTreeMap<String, Vec<AcceptedAccuracy>>,
    rejected: Vec<Rejection>,
    /// The valid candidates kept, in candidate order: every one, the best
    /// so far, or the Pareto front so far.
    kept: Vec<KeptCandidate>,
    /// The factor that calibrates the formulas' CPU time to the measured
    /// table's ([`Self::set_calibration`]); 1 without a measured table.
    pub(crate) formula_scale: f64,
    /// Under a price ranking, the cheapest price of the valid candidates
    /// offered, and the cheapest more than [`ROUNDING`] above it.
    cheapest: Option<f64>,
    runner_up: Option<f64>,
}

/// Prices within this fraction of each other are the same sum of charges
/// rounded differently (Stage 3's tie tolerance is 1e-9).
const ROUNDING: f64 = 1e-12;

/// A valid candidate the selector keeps: as Stage 2 built it, and sized
/// for its queries' failure probabilities as Stage 3 priced it.
struct KeptCandidate {
    built: PhysicalWorkloadPlan,
    sized: PhysicalWorkloadPlan,
}

impl<'a> PlanSelector<'a> {
    /// A selector for the workload `entries` over `data` under `models`.
    /// Bad settings or deployment capabilities are an error here, before
    /// any candidate.
    pub fn new(
        entries: &'a [QueryWorkloadEntry],
        data: &'a DataWorkload,
        models: PlanningModels<'a>,
        keep_every_candidate: bool,
    ) -> Result<Self, SelectionError> {
        models
            .plan_selection
            .validate()
            .map_err(SelectionError::InvalidSettings)?;
        models
            .deployment_capabilities
            .validate(data)
            .map_err(SelectionError::InvalidDeploymentCapabilities)?;
        Ok(Self {
            entries,
            data,
            models,
            targets: accuracy_targets(entries),
            recurrence: entries.iter().map(|e| e.recurrence.clone()).collect(),
            range_evaluations: entries.iter().map(|e| e.range_evaluation).collect(),
            ranking: Ranking::of(&models.plan_selection.cost_model),
            keep_every_candidate,
            candidates_checked: 0,
            costs: BTreeMap::new(),
            accepted: BTreeMap::new(),
            rejected: Vec::new(),
            kept: Vec::new(),
            formula_scale: models
                .plan_selection
                .frozen_calibration_factor
                .unwrap_or(1.0),
            cheapest: None,
            runner_up: None,
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
        self.models
            .plan_selection
            .frozen_calibration_factor
            .is_none()
            && !self
                .models
                .plan_selection
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
    ) -> Result<CalibrationFit, SelectionError> {
        if candidate.roots.len() != self.entries.len() {
            return Err(SelectionError::EntryCount {
                candidate: candidate.id.clone(),
                roots: candidate.roots.len(),
                entries: self.entries.len(),
            });
        }
        let Ok((sized, _)) = checked_candidate(candidate, &self.targets, self.data, &self.models)
        else {
            return Ok(CalibrationFit::default());
        };
        let nodes = node_map(&sized.dag);
        let kept_chains = kept_pane_chains(&sized.dag);
        if !self.models.deployment_capabilities.raw_data_retained
            && raw_data_violation(&sized.dag, &kept_chains).is_some()
        {
            return Ok(CalibrationFit::default());
        }
        let (_, fit) = price_at_scale(
            &sized.dag,
            &nodes,
            &kept_chains,
            &self.recurrence,
            &self.range_evaluations,
            self.data,
            self.models.plan_selection,
            1.0,
        )
        .map_err(|(node, error)| SelectionError::Cost {
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
            .plan_selection
            .frozen_calibration_factor
            .unwrap_or_else(|| fit.factor());
    }

    /// Check and price `candidate`, the next in candidate order, and keep
    /// it if it can still be selected.
    pub fn offer(&mut self, candidate: &PhysicalWorkloadPlan) -> Result<(), SelectionError> {
        self.offer_before(candidate, None)
    }

    /// A stopped compiler is not evidence that the candidate is invalid.
    pub fn offer_before(
        &mut self,
        candidate: &PhysicalWorkloadPlan,
        deadline: Option<std::time::Instant>,
    ) -> Result<(), SelectionError> {
        let check_deadline = || {
            if deadline.is_some_and(|limit| std::time::Instant::now() >= limit) {
                Err(SelectionError::EvaluationDeadlineExceeded)
            } else {
                Ok(())
            }
        };
        check_deadline()?;
        if candidate.roots.len() != self.entries.len() {
            return Err(SelectionError::EntryCount {
                candidate: candidate.id.clone(),
                roots: candidate.roots.len(),
                entries: self.entries.len(),
            });
        }
        self.candidates_checked += 1;
        let (sized, accepted_accuracy) =
            match checked_candidate(candidate, &self.targets, self.data, &self.models) {
                Ok(checked) => checked,
                Err(failed) => {
                    self.reject(&candidate.id, failed.cause, failed.reason);
                    return Ok(());
                }
            };
        // Built once per candidate for the raw-data check and pricing.
        let nodes = node_map(&sized.dag);
        let kept_chains = kept_pane_chains(&sized.dag);
        if !self.models.deployment_capabilities.raw_data_retained {
            if let Some(reason) = raw_data_violation(&sized.dag, &kept_chains) {
                self.reject(&candidate.id, RejectionCause::RawDataNotRetained, reason);
                return Ok(());
            }
        }
        let settings = self.models.plan_selection;
        let (cost, _) = price_at_scale(
            &sized.dag,
            &nodes,
            &kept_chains,
            &self.recurrence,
            &self.range_evaluations,
            self.data,
            settings,
            self.formula_scale,
        )
        .map_err(|(node, error)| SelectionError::Cost {
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
        self.track_runner_up(&cost);
        if !self.keep_every_candidate {
            let kept_cost = |kept: &KeptCandidate| &self.costs[&kept.built.id];
            let selectable = match self.ranking {
                // A candidate another dominates is never on the front, and
                // one it dominates leaves it.
                Ranking::Front => {
                    let latency = worst_latency_ms(&cost.usage);
                    let dominated_by = |kept: &KeptCandidate| {
                        let usage = &kept_cost(kept).usage;
                        dominates_at((usage, worst_latency_ms(usage)), (&cost.usage, latency))
                    };
                    !self.kept.iter().any(dominated_by)
                }
                // Ties keep the earlier candidate.
                _ => self
                    .kept
                    .first()
                    .is_none_or(|best| self.ranking.better(&cost, kept_cost(best))),
            };
            if !selectable {
                return Ok(());
            }
        }
        check_deadline()?;
        if !self.keep_every_candidate {
            let kept_cost = |kept: &KeptCandidate| &self.costs[&kept.built.id];
            let dominated: Vec<bool> = self
                .kept
                .iter()
                .map(|kept| match self.ranking {
                    Ranking::Front => dominates(&cost.usage, &kept_cost(kept).usage),
                    _ => true,
                })
                .collect();
            let mut dominated = dominated.into_iter();
            let (costs, accepted) = (&mut self.costs, &mut self.accepted);
            self.kept.retain(|kept| {
                let drop = dominated.next().unwrap_or(false);
                if drop {
                    costs.remove(&kept.built.id);
                    accepted.remove(&kept.built.id);
                }
                !drop
            });
        }
        self.costs.insert(candidate.id.clone(), cost);
        self.accepted
            .insert(candidate.id.clone(), accepted_accuracy);
        self.kept.push(KeptCandidate {
            built: candidate.clone(),
            sized,
        });
        Ok(())
    }

    /// Whether a candidate Stage 1 candidate `from_logical` implements is
    /// kept: it may still be selected (or every candidate is kept).
    pub fn keeps_from_logical(&self, from_logical: &str) -> bool {
        self.kept
            .iter()
            .any(|kept| kept.built.from_logical == from_logical)
    }

    /// The selection over every candidate offered, and the selected
    /// candidate as Stage 2 built it (the selection's plan is it sized for
    /// its queries' failure probabilities).
    pub fn finish(mut self) -> Result<(Selection, PhysicalWorkloadPlan), SelectionError> {
        let ranking = self.ranking;
        let valid_costs: Vec<&CandidateCost> = self
            .kept
            .iter()
            .map(|kept| &self.costs[&kept.built.id])
            .collect();
        let Some((best, front)) = choose(ranking, &valid_costs) else {
            return Err(SelectionError::NoValidCandidate(self.rejected));
        };
        let id = |index: usize| self.kept[index].built.id.clone();
        let selected = id(best);
        let front: Vec<String> = front.into_iter().map(id).collect();
        for kept in &self.kept {
            let id = &kept.built.id;
            if *id != selected {
                let (cause, reason) = ranking.lost_reason(id, &selected, &self.costs, &front);
                self.rejected.push(Rejection {
                    id: id.clone(),
                    cause,
                    reason,
                });
            }
        }
        let KeptCandidate { built, sized } = self.kept.swap_remove(best);
        let plan = with_confirmed_weights(&sized.roots, self.data);
        let pane_retention_ms = pane_retention_ms(
            &sized.dag,
            &query_reach(&sized.dag, &node_map(&sized.dag)),
            &self.range_evaluations,
        );
        let incumbent_cost = self.costs.get(&selected).and_then(|c| c.cost_per_second);
        let selection = Selection {
            selected,
            plan,
            costs: self.costs,
            accepted: self.accepted,
            rejected: self.rejected,
            front,
            pane_retention_ms,
            search: SearchReport {
                candidates_checked: self.candidates_checked,
                runner_up_cost_per_second: self.runner_up,
                incumbent_cost,
                ..SearchReport::default()
            },
        };
        Ok((selection, built))
    }

    /// Under a price ranking, fold the price of valid candidate `cost`
    /// into the cheapest price and the runner-up ([`ROUNDING`]).
    fn track_runner_up(&mut self, cost: &CandidateCost) {
        let (Ranking::Price, Some(price)) = (self.ranking, cost.cost_per_second) else {
            return;
        };
        let Some(cheapest) = self.cheapest else {
            self.cheapest = Some(price);
            return;
        };
        let rounding = ROUNDING * cheapest.abs();
        let runner_up = if price < cheapest - rounding {
            self.cheapest = Some(price);
            cheapest
        } else if price <= cheapest + rounding {
            self.cheapest = Some(cheapest.min(price));
            return;
        } else {
            price
        };
        self.runner_up = Some(self.runner_up.map_or(runner_up, |r| r.min(runner_up)));
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

/// `candidate` sized for its queries' failure probabilities, with the
/// accuracy Stage 3 accepted for each query, or the first check it fails
/// that does not depend on its materialization or prices: its resized
/// summaries do not rebuild, the executor cannot run a per-series
/// aggregate it reads, or a query misses its accuracy target.
pub(crate) fn checked_candidate(
    candidate: &PhysicalWorkloadPlan,
    targets: &[AccuracyTarget],
    data: &DataWorkload,
    models: &PlanningModels<'_>,
) -> Result<(PhysicalWorkloadPlan, Vec<AcceptedAccuracy>), CheckFailure> {
    let sized =
        sized_for_failure_probability(candidate, targets, data).map_err(|reason| CheckFailure {
            cause: RejectionCause::FailureProbabilityResizeFailed,
            reason,
            size_independent: false,
        })?;
    if let Some(reason) = per_entity_aggregate_violation(&sized.dag) {
        // The aggregate is the plan's own exact per-series reduction, which
        // no summary size changes.
        return Err(CheckFailure {
            cause: RejectionCause::PerEntityAggregateNotExecutable,
            reason,
            size_independent: true,
        });
    }
    let accepted = accuracy_check(&sized, targets, data, models)?;
    Ok((sized, accepted))
}

/// The index of the candidate `ranking` selects among `costs` (the first on
/// ties) and, under [`Ranking::Front`], the indices of the Pareto front in
/// order; `None` without candidates.
pub(crate) fn choose(ranking: Ranking, costs: &[&CandidateCost]) -> Option<(usize, Vec<usize>)> {
    let front: Vec<usize> = match ranking {
        Ranking::Front => {
            // Each candidate's worst latency once, not once per comparison.
            let latencies: Vec<f64> = costs.iter().map(|c| worst_latency_ms(&c.usage)).collect();
            (0..costs.len())
                .filter(|&i| {
                    !(0..costs.len()).any(|j| {
                        dominates_at(
                            (&costs[j].usage, latencies[j]),
                            (&costs[i].usage, latencies[i]),
                        )
                    })
                })
                .collect()
        }
        _ => Vec::new(),
    };
    // Under the Pareto front, only a front member can be selected.
    let pool: Vec<usize> = match ranking {
        Ranking::Front => front.clone(),
        _ => (0..costs.len()).collect(),
    };
    let best =
        pool.into_iter().reduce(
            |best, next| match ranking.better(costs[next], costs[best]) {
                true => next,
                false => best,
            },
        )?;
    Some((best, front))
}

/// Whether `cost` is cheaper than `best`. Totals that differ only by
/// floating-point rounding (summing the same charges in another grouping)
/// tie, and a tie keeps the earlier candidate.
pub(crate) fn cheaper(cost: f64, best: f64) -> bool {
    cost < best - best.abs() * 1e-9
}

/// The worst latency per evaluation among a candidate's queries (0 with none).
pub(crate) fn worst_latency_ms(usage: &ResourceUsage) -> f64 {
    usage
        .latency_per_query
        .iter()
        .map(QueryLatency::total_ms)
        .fold(0.0, f64::max)
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

/// Whether `a` is at least as good as `b` on peak memory, disk and
/// worst-query latency, and better on one of them.
pub(crate) fn dominates(a: &ResourceUsage, b: &ResourceUsage) -> bool {
    dominates_at((a, worst_latency_ms(a)), (b, worst_latency_ms(b)))
}

/// [`dominates`] with each usage's worst-query latency already computed.
fn dominates_at((a, la): (&ResourceUsage, f64), (b, lb): (&ResourceUsage, f64)) -> bool {
    a.peak_memory_bytes <= b.peak_memory_bytes
        && a.disk_bytes <= b.disk_bytes
        && !cheaper(lb, la)
        && (a.peak_memory_bytes < b.peak_memory_bytes
            || a.disk_bytes < b.disk_bytes
            || cheaper(la, lb))
}

/// How a cost model orders valid candidates.
#[derive(Clone, Copy)]
pub(crate) enum Ranking {
    /// Cost models 1 and 2: by price.
    Price,
    /// Latency, then peak memory, then disk.
    Latency,
    /// Peak memory, then disk, then latency.
    Memory,
    /// The Pareto front; the selected member by [`Ranking::Latency`].
    Front,
}

impl Ranking {
    pub(crate) fn of(model: &CostModel) -> Self {
        match model {
            CostModel::UsageOverTime(_) | CostModel::ProvisionedCapacity(_) => Ranking::Price,
            CostModel::ClusterCapacity(limits) => match limits.objective {
                ClusterObjective::LowestLatency => Ranking::Latency,
                ClusterObjective::LeastMemory => Ranking::Memory,
                ClusterObjective::ParetoFront => Ranking::Front,
            },
        }
    }

    /// Whether `cost` ranks strictly before `best`. Prices and latencies tie
    /// within rounding ([`cheaper`]); peak memory and disk are whole bytes,
    /// compared exactly, as [`dominates`] compares them.
    pub(crate) fn better(self, cost: &CandidateCost, best: &CandidateCost) -> bool {
        use std::cmp::Ordering;
        let rounded = |a: f64, b: f64| match (cheaper(a, b), cheaper(b, a)) {
            (true, _) => Ordering::Less,
            (_, true) => Ordering::Greater,
            _ => Ordering::Equal,
        };
        let (a, b) = (&cost.usage, &best.usage);
        let latency = || rounded(worst_latency_ms(a), worst_latency_ms(b));
        let memory = || a.peak_memory_bytes.cmp(&b.peak_memory_bytes);
        let disk = || a.disk_bytes.cmp(&b.disk_bytes);
        let order = match self {
            Ranking::Price => rounded(
                cost.cost_per_second.unwrap_or(f64::INFINITY),
                best.cost_per_second.unwrap_or(f64::INFINITY),
            ),
            Ranking::Latency | Ranking::Front => latency().then_with(memory).then_with(disk),
            Ranking::Memory => memory().then_with(disk).then_with(latency),
        };
        order == Ordering::Less
    }

    /// Why valid candidate `id` was not `selected`, and the reason with
    /// its numbers.
    fn lost_reason(
        self,
        id: &str,
        selected: &str,
        costs: &BTreeMap<String, CandidateCost>,
        front: &[String],
    ) -> (RejectionCause, String) {
        let (cost, best) = (&costs[id], &costs[selected]);
        if !self.better(best, cost) {
            let tied = match self {
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
        match self {
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
            Ranking::Front if front.iter().any(|f| f == id) => (
                RejectionCause::NotLowestLatencyOnParetoFront,
                format!(
                    "on the Pareto front, not its lowest-latency member: {} vs {}",
                    describe_usage(&cost.usage),
                    describe_usage(&best.usage)
                ),
            ),
            Ranking::Front => {
                let by = front
                    .iter()
                    .find(|f| dominates(&costs[f.as_str()].usage, &cost.usage))
                    .map_or(selected, String::as_str);
                (
                    RejectionCause::Dominated,
                    format!(
                        "dominated by {by}: {} vs {}",
                        describe_usage(&cost.usage),
                        describe_usage(&costs[by].usage)
                    ),
                )
            }
        }
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
