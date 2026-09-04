//! Recurrence-aware cost context (issue #287).
//!
//! ASAPPlanner already models recurring-workload metadata
//! ([`asap_types::workload::RepeatingEntry`]) and ingest-rate metadata
//! ([`asap_types::workload::DataWorkload`]), but until this module
//! neither reached [`CostModel`]'s CSE share-vs-recompute decision
//! ([`CostModel::cse_share_decision`]): that decision only ever compared a
//! *structural* consumer count (how many workload locations reference a
//! shared subtree) against a flat per-family maintenance weight — it had no
//! notion of how *often* those consumers actually run.
//!
//! This module adds that notion as a generic cost context, not a scheduler:
//! no Prometheus rule-group semantics, no execution loop, no temporal-pane
//! boundary reasoning (see the module's own "Out of scope" list mirrored
//! from the issue).
//!
//! ## Units — every new cost input names its own unit explicitly
//!
//! | Type | Unit | Meaning |
//! |---|---|---|
//! | [`UpdateRate`] | Hz (updates/second) | how often the *raw* data underlying a maintained summary changes (ingest rate) |
//! | [`EvaluationRate`] | Hz (evaluations/second) | how often a target is *read* — `sum(1 / query_interval_i)` over every repeating consumer |
//! | [`CostRate`] | cost units / second | a steady-state cost rate — never comparable to a bare [`Cost`](crate::cost_model::Cost) without going through [`total_cost`] |
//! | [`Horizon`] | seconds | the explicit evaluation window a caller supplies to compare a rate-valued cost against a one-shot cost |
//!
//! `Cost` (bare, from [`crate::cost_model`]) stays a one-time, unitless
//! magnitude — exactly what it was before this module existed, preserved
//! for [`CostModel::cse_share_decision`] and everything else that already
//! uses it. `CostRate` is a *new*, distinct type specifically so a rate and
//! a one-shot cost can never be added directly (no `impl Add<Cost> for
//! CostRate`, and vice versa) — the compiler enforces the issue's "must not
//! silently combine rate-valued and one-shot costs" requirement; [`total_cost`]
//! is the one sanctioned way to combine them, and it takes an explicit
//! [`Horizon`] to do it.
//!
//! ## Cost semantics (from the issue)
//!
//! For a maintained summary:
//!
//! ```text
//! maintained_cost_rate =
//!     update_rate * maintenance_cost_per_update
//!   + evaluation_rate * summary_read_cost
//! ```
//!
//! For recomputation from the pre-ASAP/raw path:
//!
//! ```text
//! recompute_cost_rate = evaluation_rate * raw_recompute_cost
//! ```
//!
//! For a summary shared by multiple repeating consumers with intervals
//! `t1..tn`:
//!
//! ```text
//! evaluation_rate = sum(1 / query_interval_i)
//! ```
//! ([`evaluation_rate_of`].)
//!
//! Repetition amortizes a maintained summary across more reads; it does
//! *not* reduce the physical maintenance work caused by ingest updates —
//! that's why `update_rate` and `evaluation_rate` are two separate terms in
//! `maintained_cost_rate` above, never folded into one.
//!
//! One-shot consumers are represented separately from a steady-state rate
//! ([`RecurrenceProfile::one_shot_consumers`]). Comparing a mix of one-shot
//! and repeating work requires an explicit evaluation horizon `H`:
//!
//! ```text
//! total_cost(H) = recurring_cost_rate * H + one_shot_cost
//! ```
//! ([`total_cost`].)
//!
//! ## Provenance of each new cost input
//!
//! - [`EvaluationRate`]: derived from [`asap_types::workload::RepeatingEntry::demand`]
//!   values of every repeating consumer reaching a target (via
//!   [`evaluation_rate_of`], or [`crate::replacement::PlanSpace::recurrence_profiles`]
//!   for a whole workload). A one-shot ([`asap_types::workload::BatchEntry`])
//!   consumer contributes to [`RecurrenceProfile::one_shot_consumers`]
//!   instead, never to this rate.
//! - [`UpdateRate`]: read from workload-level
//!   [`asap_types::workload::DataWorkload::ingestion_rate`] via
//!   [`update_rate_from_data_workload`]. Missing evidence remains unknown;
//!   data at rest is not assigned a fabricated update rate.
//! - `maintenance_cost_per_update` / `summary_read_cost` /
//!   `raw_recompute_cost`: [`CostModel`] hooks (defaults documented on the
//!   trait itself, in `cost_model.rs`) — illustrative placeholders, like
//!   every other default in that trait; a deployment with real numbers
//!   overrides them.
//!
//! ## Preserving existing behavior when recurrence metadata is unavailable
//!
//! [`RecurrenceProfile::is_empty`] is `true` exactly when a caller supplied
//! no [`RepeatingEntry`](asap_types::workload::RepeatingEntry)/
//! [`DataWorkload`](asap_types::workload::DataWorkload)-derived
//! information at all (no evaluation rate, no update rate, zero recorded
//! one-shot consumers — [`RecurrenceProfile::EMPTY`], its `Default`).
//! [`CostModel::cse_share_decision_with_recurrence`]'s default body checks
//! this first and, when true, delegates to
//! [`CostModel::cse_share_decision`] byte-for-byte — the existing,
//! structural-consumer-count decision this module never has to touch or
//! second-guess when there's nothing new to feed it.

use std::fmt;

use asap_types::workload::{DataWorkload, RepetitionInterval};

use crate::cost_model::{Cost, CostModel, CseCandidate, ShareDecision};

// ── Units ────────────────────────────────────────────────────────────────

/// How often the *raw* data underlying a maintained summary changes —
/// ingest/update events per second (Hz). See the module docs' provenance
/// table: normally read from [`DataWorkload::ingestion_rate`] via
/// [`update_rate_from_data_workload`].
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
pub struct UpdateRate(pub f64);

/// How often a target is *evaluated* by its consumers — evaluations per
/// second (Hz). For a summary shared by repeating consumers with intervals
/// `t1..tn`, `sum(1 / t_i)` ([`evaluation_rate_of`]); a one-shot consumer
/// never contributes to this rate (see [`RecurrenceProfile::one_shot_consumers`]).
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
pub struct EvaluationRate(pub f64);

/// A steady-state cost rate: cost units per second. Deliberately a
/// different type from [`Cost`] (a one-time, unitless magnitude) — there is
/// no `impl Add<Cost> for CostRate` on purpose, so a rate and a one-shot
/// cost can never be combined except explicitly, through [`total_cost`].
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
pub struct CostRate(pub f64);

impl CostRate {
    /// A cost rate of exactly zero (no ongoing cost at all).
    pub const ZERO: CostRate = CostRate(0.0);
}

impl fmt::Display for CostRate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/s", self.0)
    }
}

impl std::ops::Add for CostRate {
    type Output = CostRate;
    fn add(self, rhs: CostRate) -> CostRate {
        CostRate(self.0 + rhs.0)
    }
}

/// An explicit evaluation horizon, in seconds — the only input that lets a
/// [`CostRate`] be combined with a one-shot [`Cost`] (via [`total_cost`]).
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
pub struct Horizon(pub f64);

/// The one sanctioned way to combine a steady-state [`CostRate`] with a
/// one-shot [`Cost`]: `rate * horizon + one_shot`. There is no other path
/// in this module (or in [`CostModel`]) that adds a `CostRate` to a `Cost`
/// — every other cost value stays in exactly one of the two units,
/// enforced by the type system, satisfying issue #287's "the cost model
/// must not silently combine rate-valued and one-shot costs" requirement.
pub fn total_cost(rate: CostRate, horizon: Horizon, one_shot: Cost) -> Cost {
    Cost(rate.0 * horizon.0 + one_shot.0)
}

// ── Errors ───────────────────────────────────────────────────────────────

/// Errors from building or applying recurrence-aware cost inputs.
#[derive(Debug, Clone, Copy, PartialEq, thiserror::Error)]
pub enum RecurrenceError {
    /// A [`RepetitionInterval`] of zero (or, in principle, any non-positive
    /// value — `RepetitionInterval` is `u32`-backed so only zero is
    /// representable) was supplied. A zero interval has no finite rate
    /// (`1 / 0`), so it cannot contribute to an [`EvaluationRate`].
    #[error(
        "invalid RepetitionInterval({0:?}ms): a repeating query's interval must be > 0 to \
         contribute a finite evaluation rate"
    )]
    InvalidInterval(RepetitionInterval),
    /// A comparison mixed one-shot and repeating work
    /// ([`RecurrenceProfile::one_shot_consumers`] > 0 alongside a non-empty
    /// [`RecurrenceProfile::evaluation_rate`] or [`RecurrenceProfile::update_rate`])
    /// without an explicit [`Horizon`] to combine them — see [`total_cost`]
    /// and the module docs' "Cost semantics" section.
    #[error(
        "comparison mixes one-shot and repeating work but no evaluation horizon was supplied; \
         an explicit Horizon is required to combine a CostRate with a one-shot Cost (see \
         recurrence::total_cost)"
    )]
    MissingHorizon,
    /// An [`UpdateRate`] that isn't finite and non-negative (NaN, infinite,
    /// or negative) was supplied — such a value would silently corrupt
    /// every downstream comparison (a NaN rate makes every `<=`/`>`
    /// comparison `false`, which [`decide`] would otherwise read as "always
    /// recompute" with no diagnostic at all). Validated the same way
    /// [`evaluation_rate_of`] validates a zero [`RepetitionInterval`].
    #[error(
        "invalid UpdateRate({0:?}Hz): an update rate must be finite and >= 0 to contribute a \
         well-defined maintained_cost_rate"
    )]
    InvalidUpdateRate(UpdateRate),
    #[error("invalid EvaluationRate({0:?}Hz): an evaluation rate must be finite and >= 0")]
    InvalidEvaluationRate(EvaluationRate),
    #[error(transparent)]
    InvalidWorkload(#[from] asap_types::workload::WorkloadError),
    #[error("workload entry index {index} is out of bounds for {entry_count} entries")]
    InvalidWorkloadEntry { index: usize, entry_count: usize },
    /// A [`Horizon`] that isn't finite and strictly positive (NaN,
    /// infinite, zero, or negative) was supplied — a non-positive or
    /// infinite horizon would silently drop or invert the recurring
    /// `CostRate` term in [`total_cost`], exactly the "silently combine"
    /// outcome [`MissingHorizon`](Self::MissingHorizon) exists to prevent.
    #[error(
        "invalid Horizon({0:?}s): an evaluation horizon must be finite and > 0 to combine a \
         CostRate with a one-shot Cost without distorting the comparison"
    )]
    InvalidHorizon(Horizon),
    /// [`crate::replacement::PlanSpace::recurrence_profiles`] was called
    /// with a `root_recurrence` slice whose length doesn't match the
    /// `PlanSpace`'s own root count — a caller error, but recoverable
    /// (this method's whole signature promises a `Result`, so this is
    /// reported the same way every other input-validation failure is,
    /// never a panic).
    #[error(
        "recurrence_profiles: root_recurrence must have one entry per root, in the same order \
         PlanSpace::roots is in (got {got} entries for {expected} roots)"
    )]
    RootCountMismatch {
        /// `PlanSpace::roots.len()`.
        expected: usize,
        /// `root_recurrence.len()`.
        got: usize,
    },
}

/// Reject a non-finite or negative [`UpdateRate`] — the same validation
/// discipline [`evaluation_rate_of`] applies to each [`RepetitionInterval`],
/// applied at every point an `UpdateRate` enters a [`RecurrenceProfile`]
/// ([`RecurrenceProfile::with_update_rate`],
/// [`update_rate_from_data_workload`],
/// [`crate::replacement::PlanSpace::recurrence_profiles`]'s own parameter)
/// *and*, as a backstop that can't be bypassed by constructing a
/// `RecurrenceProfile` via its public fields directly, inside [`decide`]
/// itself before any comparison uses it.
pub fn validate_update_rate(rate: UpdateRate) -> Result<UpdateRate, RecurrenceError> {
    if rate.0.is_finite() && rate.0 >= 0.0 {
        Ok(rate)
    } else {
        Err(RecurrenceError::InvalidUpdateRate(rate))
    }
}

/// Reject a non-finite or non-positive [`Horizon`] — validated inside
/// [`decide`] wherever a caller-supplied `horizon` is used, so `Horizon(0.0)`
/// or a negative horizon can't silently zero out or invert the recurring
/// `CostRate` term in [`total_cost`].
pub fn validate_horizon(horizon: Horizon) -> Result<Horizon, RecurrenceError> {
    if horizon.0.is_finite() && horizon.0 > 0.0 {
        Ok(horizon)
    } else {
        Err(RecurrenceError::InvalidHorizon(horizon))
    }
}

// ── Aggregation ──────────────────────────────────────────────────────────

/// `sum(1 / interval_i)`, converted from milliseconds
/// ([`RepetitionInterval`]'s own unit) to Hz, over every repeating
/// consumer's interval. `Ok(None)` when `intervals` is empty — "no
/// repeating consumers observed", distinct from "observed consumers whose
/// rate happens to be zero" (impossible: every valid interval contributes a
/// strictly positive rate). `Err` on the first zero interval encountered.
pub fn evaluation_rate_of<I>(intervals: I) -> Result<Option<EvaluationRate>, RecurrenceError>
where
    I: IntoIterator<Item = RepetitionInterval>,
{
    let mut total_hz = 0.0;
    let mut any = false;
    for interval in intervals {
        if interval.0 == 0 {
            return Err(RecurrenceError::InvalidInterval(interval));
        }
        any = true;
        // RepetitionInterval is in milliseconds; Hz = 1000 / ms.
        total_hz += 1000.0 / f64::from(interval.0);
    }
    Ok(any.then_some(EvaluationRate(total_hz)))
}

/// Read an [`UpdateRate`] from workload-level [`DataWorkload`] evidence.
/// Missing evidence remains `None`; a present non-finite or negative rate is
/// rejected rather than propagated into a [`RecurrenceProfile`].
pub fn update_rate_from_data_workload(
    workload: &DataWorkload,
) -> Result<Option<UpdateRate>, RecurrenceError> {
    workload
        .ingestion_rate
        .value
        .map(|rate| validate_update_rate(UpdateRate(rate.0)))
        .transpose()
}

// ── RecurrenceProfile ────────────────────────────────────────────────────

/// Aggregated recurrence context for one [`CseCandidate`]'s shared target:
/// how fast it's evaluated, how many one-shot consumers reference it
/// separately, and how fast its underlying raw data updates.
///
/// [`RecurrenceProfile::EMPTY`] (also its `Default`) represents "no
/// recurrence metadata available at all" — the case
/// [`CostModel::cse_share_decision_with_recurrence`]'s default body
/// recognizes via [`is_empty`](Self::is_empty) and treats as "preserve
/// existing (structural) behavior".
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct RecurrenceProfile {
    /// `sum(1 / interval_i)` over every repeating consumer of this target,
    /// in Hz. `None` when no repeating consumer references it.
    pub evaluation_rate: Option<EvaluationRate>,
    /// How many one-shot (batch) consumers reference this target,
    /// independent of `evaluation_rate` — see the module docs' "Cost
    /// semantics" section on why these can't be merged into one rate.
    pub one_shot_consumers: usize,
    /// The ingest/update rate of the raw data this target (if maintained)
    /// would be kept up to date against. `None` when no
    /// [`DataWorkload::ingestion_rate`] evidence was available.
    pub update_rate: Option<UpdateRate>,
}

impl RecurrenceProfile {
    /// No recurrence metadata at all: no evaluation rate, no one-shot
    /// consumers, no update rate.
    pub const EMPTY: RecurrenceProfile = RecurrenceProfile {
        evaluation_rate: None,
        one_shot_consumers: 0,
        update_rate: None,
    };

    /// Whether this profile carries no recurrence information at all — the
    /// "missing metadata" case [`CostModel::cse_share_decision_with_recurrence`]
    /// falls back on.
    pub fn is_empty(&self) -> bool {
        self.evaluation_rate.is_none() && self.one_shot_consumers == 0 && self.update_rate.is_none()
    }

    /// Build a profile purely from a set of repeating consumers' intervals
    /// (no one-shot consumers, no update rate — attach those with
    /// [`with_one_shot_consumers`](Self::with_one_shot_consumers)/
    /// [`with_update_rate`](Self::with_update_rate)).
    pub fn from_repeating_intervals(
        intervals: impl IntoIterator<Item = RepetitionInterval>,
    ) -> Result<Self, RecurrenceError> {
        Ok(Self {
            evaluation_rate: evaluation_rate_of(intervals)?,
            ..Self::EMPTY
        })
    }

    /// Attach a count of one-shot consumers.
    pub fn with_one_shot_consumers(mut self, one_shot_consumers: usize) -> Self {
        self.one_shot_consumers = one_shot_consumers;
        self
    }

    /// Attach an ingest/update rate (from [`update_rate_from_data_workload`]
    /// or a deployment-specific measurement). Validated via
    /// [`validate_update_rate`] — rejects a NaN, infinite, or negative rate
    /// rather than silently storing it.
    pub fn with_update_rate(mut self, update_rate: UpdateRate) -> Result<Self, RecurrenceError> {
        self.update_rate = Some(validate_update_rate(update_rate)?);
        Ok(self)
    }
}

/// How one workload root recurs — the opaque per-root tag
/// [`crate::replacement::PlanSpace::recurrence_profiles`] threads down to
/// every target reachable from that root. Mirrors
/// [`asap_types::workload::QueryWorkload`]'s own `query_batch` (one-shot)
/// vs. `repeating_queries` (an interval each) split, but at the
/// already-opaque `Id` granularity `search_workload`'s callers already use
/// — this crate needs no more of a caller's own query identity than "which
/// of these two recurrence kinds is this root". One-time roots always use a
/// count, so there is no second spelling for the common count-of-one case.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RootRecurrence {
    /// A declared number of one-time invocations for this root.
    OneShotCount(usize),
    /// A repeated root normalized to evaluations per second.
    Repeating(EvaluationRate),
    /// No reliable recurrence evidence was supplied. It contributes no read
    /// count or evaluation rate, but remains distinct from zero demand.
    Unknown,
}

// ── Explanation ──────────────────────────────────────────────────────────

/// The full readout [`CostModel::cse_share_decision_with_recurrence`]
/// returns: which alternative was selected, both compared cost rates
/// (and, when a [`Horizon`] was supplied, both compared totals), every
/// input that went into them, their units, and provenance — meant to be
/// both machine-consumable (e.g. by issue #286's DAG-viewer cost/benefit
/// annotations) and human-readable (via its [`fmt::Display`] impl).
#[derive(Debug, Clone, PartialEq)]
pub struct RecurrenceCostExplanation {
    /// The selected alternative.
    pub decision: ShareDecision,
    /// `maintained_cost_rate` — cost units/second — as defined in the
    /// module docs' "Cost semantics" section. `None` on the structural
    /// fallback path (`RecurrenceProfile::is_empty()`): no rate was
    /// computed at all there (the decision came from
    /// [`CostModel::cse_share_decision`] instead), so this is "not
    /// computed", deliberately distinct from a real, computed rate of
    /// exactly zero.
    pub maintained_cost_rate: Option<CostRate>,
    /// `recompute_cost_rate` — cost units/second. `None` on the same
    /// structural fallback path, for the same reason.
    pub recompute_cost_rate: Option<CostRate>,
    /// `total_cost(horizon)` for the maintained alternative, when `horizon`
    /// is `Some`.
    pub maintained_total: Option<Cost>,
    /// `total_cost(horizon)` for the recompute alternative, when `horizon`
    /// is `Some`.
    pub recompute_total: Option<Cost>,
    /// The horizon the totals above were computed over, if any.
    pub horizon: Option<Horizon>,
    /// The [`UpdateRate`] input used, if any.
    pub update_rate: Option<UpdateRate>,
    /// The [`EvaluationRate`] input used, if any.
    pub evaluation_rate: Option<EvaluationRate>,
    /// The one-shot consumer count input used.
    pub one_shot_consumers: usize,
    /// `maintenance_cost_per_update` — cost units/update. `None` on the
    /// structural fallback path (not computed there).
    pub maintenance_cost_per_update: Option<Cost>,
    /// `summary_read_cost` — cost units/read. `None` on the structural
    /// fallback path.
    pub summary_read_cost: Option<Cost>,
    /// `raw_recompute_cost` — cost units/recomputation. `None` on the
    /// structural fallback path.
    pub raw_recompute_cost: Option<Cost>,
    /// `summary_build_cost` — one-time cost of materializing the maintained
    /// summary. `None` on the structural fallback path.
    pub summary_build_cost: Option<Cost>,
    /// Human-readable provenance: which model/path produced this
    /// explanation (e.g. "recurrence-aware: <CostModel type>" or the
    /// structural fallback note when `RecurrenceProfile::is_empty()`).
    pub provenance: String,
}

impl fmt::Display for RecurrenceCostExplanation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "decision: {:?} ({})", self.decision, self.provenance)?;
        match (self.maintained_cost_rate, self.recompute_cost_rate) {
            (Some(maintained), Some(recompute)) => {
                writeln!(
                    f,
                    "  maintained_cost_rate = {} (update_rate={:?}Hz * \
                     maintenance_cost_per_update={:?} + evaluation_rate={:?}Hz * \
                     summary_read_cost={:?})",
                    maintained,
                    self.update_rate.map(|r| r.0),
                    self.maintenance_cost_per_update,
                    self.evaluation_rate.map(|r| r.0),
                    self.summary_read_cost,
                )?;
                writeln!(
                    f,
                    "  recompute_cost_rate  = {} (evaluation_rate={:?}Hz * \
                     raw_recompute_cost={:?})",
                    recompute,
                    self.evaluation_rate.map(|r| r.0),
                    self.raw_recompute_cost,
                )?;
            }
            _ => {
                writeln!(
                    f,
                    "  maintained_cost_rate / recompute_cost_rate: not computed (structural \
                     fallback — no recurrence metadata was supplied)"
                )?;
            }
        }
        writeln!(f, "  one_shot_consumers = {}", self.one_shot_consumers)?;
        if let Some(h) = self.horizon {
            writeln!(
                f,
                "  horizon = {}s; summary_build_cost = {:?}; maintained_total = {:?}, \
                 recompute_total = {:?}",
                h.0, self.summary_build_cost, self.maintained_total, self.recompute_total
            )?;
        }
        Ok(())
    }
}

/// [`CostModel::cse_share_decision_with_recurrence`]'s default body — see
/// that method's own doc for the decision rule; kept as a free function so
/// the logic exists exactly once regardless of how many `CostModel`
/// implementors inherit the default.
pub(crate) fn decide<C: CostModel + ?Sized>(
    cost_model: &C,
    candidate: &CseCandidate,
    recurrence: &RecurrenceProfile,
    horizon: Option<Horizon>,
) -> Result<RecurrenceCostExplanation, RecurrenceError> {
    // Backstop validation: a `RecurrenceProfile`/`Horizon` may have reached
    // here via a direct struct literal (bypassing `with_update_rate`'s own
    // check) or a caller-supplied `horizon` argument — this is the one
    // choke point every path funnels through before a value actually enters
    // a comparison, so it's validated here regardless of how it arrived.
    if let Some(rate) = recurrence.update_rate {
        validate_update_rate(rate)?;
    }
    if let Some(h) = horizon {
        validate_horizon(h)?;
    }

    if recurrence.is_empty() {
        let decision = cost_model.cse_share_decision(candidate);
        return Ok(RecurrenceCostExplanation {
            decision,
            maintained_cost_rate: None,
            recompute_cost_rate: None,
            maintained_total: None,
            recompute_total: None,
            horizon: None,
            update_rate: None,
            evaluation_rate: None,
            one_shot_consumers: 0,
            maintenance_cost_per_update: None,
            summary_read_cost: None,
            raw_recompute_cost: None,
            summary_build_cost: None,
            provenance: "structural fallback: no recurrence metadata supplied — delegated to \
                         CostModel::cse_share_decision (consumer_count-based), preserving \
                         pre-#287 behavior exactly"
                .to_string(),
        });
    }

    let has_recurring = recurrence.evaluation_rate.is_some() || recurrence.update_rate.is_some();
    if recurrence.one_shot_consumers > 0 && has_recurring && horizon.is_none() {
        return Err(RecurrenceError::MissingHorizon);
    }

    let update_rate = recurrence.update_rate.map_or(0.0, |r| r.0);
    let evaluation_rate = recurrence.evaluation_rate.map_or(0.0, |r| r.0);

    let maintenance_cost_per_update = cost_model.maintenance_cost_per_update(candidate);
    let summary_read_cost = cost_model.summary_read_cost(candidate);
    let raw_recompute_cost = cost_model.raw_recompute_cost(candidate);
    // The one-time cost of materializing the shared summary at all, before
    // any read or update — see `CostModel::summary_build_cost`'s own doc.
    // Without this term, a purely (or mostly) one-shot comparison modeled
    // "maintained" as free to construct, so `Share` won unconditionally
    // for *any* number of one-shot consumers (issue #287 review, bug 1) —
    // this term is what makes materializing-and-reading actually cost more
    // than a single direct recompute for a lone consumer, while still
    // amortizing correctly across many.
    let summary_build_cost = cost_model.summary_build_cost(candidate);

    let maintained_cost_rate = CostRate(
        update_rate * maintenance_cost_per_update.0 + evaluation_rate * summary_read_cost.0,
    );
    let recompute_cost_rate = CostRate(evaluation_rate * raw_recompute_cost.0);

    // A pure one-shot comparison (no recurring rate at all — `has_recurring`
    // is false, so the `MissingHorizon` gate above never fired even though
    // `one_shot_consumers > 0`) still needs *some* horizon to run
    // `total_cost` through, or its one-shot costs would never enter the
    // decision at all. Any positive horizon gives the same ordering here,
    // since `maintained_cost_rate`/`recompute_cost_rate` are both exactly
    // zero in this case (`rate * H` contributes nothing regardless of `H`)
    // — so an implicit `Horizon(1.0)` is exact, not approximate, and this
    // is never reached for the genuinely mixed case (that already required
    // an explicit `horizon` above).
    let effective_horizon = horizon
        .or_else(|| (recurrence.one_shot_consumers > 0 && !has_recurring).then_some(Horizon(1.0)));

    let (maintained_total, recompute_total, decision) = if let Some(h) = effective_horizon {
        // `summary_build_cost` is paid exactly once — whether the summary
        // is ever read again by a repeating consumer or not — never scaled
        // by `one_shot_consumers`.
        let one_shot_maintained =
            Cost(summary_read_cost.0 * recurrence.one_shot_consumers as f64) + summary_build_cost;
        let one_shot_recompute = Cost(raw_recompute_cost.0 * recurrence.one_shot_consumers as f64);
        let maintained = total_cost(maintained_cost_rate, h, one_shot_maintained);
        let recompute = total_cost(recompute_cost_rate, h, one_shot_recompute);
        let decision = if maintained.0 <= recompute.0 {
            ShareDecision::Share
        } else {
            ShareDecision::RecomputeIndependently
        };
        (Some(maintained), Some(recompute), decision)
    } else {
        // Pure recurring, no one-shot consumers at all: comparing the bare
        // rates is exactly equivalent to comparing `rate * H` for any fixed
        // `H > 0` in the limit of a long-lived, continuously-maintained
        // summary, so no horizon is needed to get a correct decision — the
        // one-time `summary_build_cost` is asymptotically negligible next
        // to an ongoing rate term and is deliberately not charged here (it
        // only enters the comparison when a caller actually needs an
        // absolute total over a finite horizon, via the branch above).
        let decision = if maintained_cost_rate.0 <= recompute_cost_rate.0 {
            ShareDecision::Share
        } else {
            ShareDecision::RecomputeIndependently
        };
        (None, None, decision)
    };

    Ok(RecurrenceCostExplanation {
        decision,
        maintained_cost_rate: Some(maintained_cost_rate),
        recompute_cost_rate: Some(recompute_cost_rate),
        maintained_total,
        recompute_total,
        horizon: effective_horizon,
        update_rate: recurrence.update_rate,
        evaluation_rate: recurrence.evaluation_rate,
        one_shot_consumers: recurrence.one_shot_consumers,
        maintenance_cost_per_update: Some(maintenance_cost_per_update),
        summary_read_cost: Some(summary_read_cost),
        raw_recompute_cost: Some(raw_recompute_cost),
        summary_build_cost: Some(summary_build_cost),
        provenance: "recurrence-aware: maintained_cost_rate vs recompute_cost_rate (cost \
                     units/second), per issue #287"
            .to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cost_model::DefaultCostModel;

    fn interval(ms: u32) -> RepetitionInterval {
        RepetitionInterval(ms)
    }

    fn repeating(ms: u32) -> RootRecurrence {
        RootRecurrence::Repeating(evaluation_rate_of([interval(ms)]).unwrap().unwrap())
    }

    // ── evaluation_rate_of ──────────────────────────────────────────────

    #[test]
    fn evaluation_rate_of_empty_is_none() {
        assert_eq!(evaluation_rate_of(vec![]).unwrap(), None);
    }

    #[test]
    fn evaluation_rate_of_single_interval() {
        // 1000ms interval => 1 Hz.
        let rate = evaluation_rate_of(vec![interval(1000)]).unwrap().unwrap();
        assert!((rate.0 - 1.0).abs() < 1e-9);
    }

    #[test]
    fn evaluation_rate_of_mixed_intervals_sums_reciprocals() {
        // 1s, 10s, 100s intervals => 1 + 0.1 + 0.01 Hz.
        let rate = evaluation_rate_of(vec![interval(1_000), interval(10_000), interval(100_000)])
            .unwrap()
            .unwrap();
        assert!((rate.0 - 1.11).abs() < 1e-9, "rate={}", rate.0);
    }

    #[test]
    fn evaluation_rate_of_rejects_zero_interval() {
        let err = evaluation_rate_of(vec![interval(1000), interval(0)]).unwrap_err();
        assert_eq!(err, RecurrenceError::InvalidInterval(interval(0)));
    }

    // ── update_rate_from_data_workload ───────────────────────────────────

    #[test]
    fn update_rate_from_data_workload_reads_ingestion_rate_evidence() {
        let workload = DataWorkload {
            ingestion_rate: asap_types::workload::Evidence {
                value: Some(asap_types::workload::Rate(100.0)),
                ..Default::default()
            },
            ..Default::default()
        };
        let rate = update_rate_from_data_workload(&workload).unwrap().unwrap();
        assert!((rate.0 - 100.0).abs() < 1e-9);
    }

    #[test]
    fn update_rate_from_data_workload_rejects_a_negative_rate() {
        let workload = DataWorkload {
            ingestion_rate: asap_types::workload::Evidence {
                value: Some(asap_types::workload::Rate(-0.1)),
                ..Default::default()
            },
            ..Default::default()
        };
        let err = update_rate_from_data_workload(&workload).unwrap_err();
        assert!(matches!(err, RecurrenceError::InvalidUpdateRate(_)));
    }

    #[test]
    fn update_rate_from_data_workload_preserves_missing_evidence() {
        assert_eq!(
            update_rate_from_data_workload(&DataWorkload::default()).unwrap(),
            None
        );
    }

    // ── RecurrenceProfile ─────────────────────────────────────────────────

    #[test]
    fn empty_profile_is_empty() {
        assert!(RecurrenceProfile::EMPTY.is_empty());
        assert!(RecurrenceProfile::default().is_empty());
    }

    #[test]
    fn profile_with_any_field_set_is_not_empty() {
        assert!(!RecurrenceProfile::EMPTY
            .with_one_shot_consumers(1)
            .is_empty());
        assert!(!RecurrenceProfile::EMPTY
            .with_update_rate(UpdateRate(1.0))
            .unwrap()
            .is_empty());
        assert!(
            !RecurrenceProfile::from_repeating_intervals(vec![interval(1000)])
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn with_update_rate_rejects_nan_infinite_and_negative() {
        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -1.0] {
            let err = RecurrenceProfile::EMPTY
                .with_update_rate(UpdateRate(bad))
                .unwrap_err();
            assert!(
                matches!(err, RecurrenceError::InvalidUpdateRate(_)),
                "bad={bad}, err={err:?}"
            );
        }
        // Zero is a legitimate (if unusual) update rate: no ingest at all.
        assert!(RecurrenceProfile::EMPTY
            .with_update_rate(UpdateRate(0.0))
            .is_ok());
    }

    #[test]
    fn from_repeating_intervals_rejects_zero_interval() {
        let err = RecurrenceProfile::from_repeating_intervals(vec![interval(0)]).unwrap_err();
        assert_eq!(err, RecurrenceError::InvalidInterval(interval(0)));
    }

    // ── total_cost / unit consistency ────────────────────────────────────

    #[test]
    fn total_cost_combines_rate_and_one_shot_explicitly() {
        let rate = CostRate(2.0);
        let horizon = Horizon(10.0);
        let one_shot = Cost(5.0);
        assert_eq!(total_cost(rate, horizon, one_shot), Cost(25.0));
    }

    #[test]
    fn cost_rate_and_cost_are_distinct_types() {
        // Compile-time unit-consistency check: `CostRate` has no `Add<Cost>`
        // or `From<Cost>` — the only way shown here to combine them is
        // `total_cost`, which forces an explicit `Horizon`. (This test's
        // real assertion is that the crate compiles at all with no such
        // impls; the runtime assertion below just exercises the intended
        // combination path.)
        let combined = total_cost(CostRate(1.0), Horizon(1.0), Cost(1.0));
        assert_eq!(combined, Cost(2.0));
    }

    // ── decide (structural fallback) ─────────────────────────────────────

    use crate::cost_model::CseCandidate;
    use asap_types::post_asap::{
        ExactKind, ExactParams, GroupingStrategy, ResultGuarantee, SummaryExpr, SummaryFamilyType,
        SummaryField, SummaryNode, SummarySchema,
    };
    use asap_types::pre_asap::expr_ir::ColumnRef;
    use asap_types::pre_asap::query_expr::{QueryExpr, Reduction, Source};
    use asap_types::pre_asap::schema::{Column, DataType, Schema};
    use std::rc::Rc;

    fn scan() -> QueryExpr {
        QueryExpr::Scan {
            source: Source::TimeSeries { metric: "m".into() },
            predicates: vec![],
            schema: Schema::with_time_index(
                vec![
                    Column::new("ts", DataType::Timestamp, false),
                    Column::new("value", DataType::Float64, false),
                ],
                0,
                vec![],
            ),
        }
    }

    fn summary_node(family: SummaryFamilyType) -> SummaryNode {
        SummaryNode {
            expr: SummaryExpr::SummaryAgg {
                child: Rc::new(SummaryNode {
                    expr: SummaryExpr::KeepPreAsap(Rc::new(scan())),
                    schema: SummarySchema {
                        fields: vec![],
                        time_index: None,
                    },
                    guarantee: Some(ResultGuarantee::exact("KeepPreAsap")),
                }),
                family: family.clone(),
                input: asap_types::post_asap::SummaryInput::column(ColumnRef::Named(
                    "value".into(),
                )),
                reduction: Reduction::by(vec![]),
                grouping: GroupingStrategy::default(),
            },
            schema: SummarySchema {
                fields: vec![SummaryField {
                    name: "state".into(),
                    dtype: family,
                    nullable: false,
                }],
                time_index: None,
            },
            guarantee: None,
        }
    }

    #[test]
    fn decide_falls_back_to_structural_decision_when_profile_is_empty() {
        let subtree = scan();
        let bound = summary_node(SummaryFamilyType::ExactAggregate(
            ExactKind::Sum,
            ExactParams::Sum,
        ));
        let candidate = CseCandidate {
            subtree: &subtree,
            bound_summary: &bound,
            consumer_count: 1000,
        };
        let explanation = decide(
            &DefaultCostModel,
            &candidate,
            &RecurrenceProfile::EMPTY,
            None,
        )
        .unwrap();
        assert_eq!(
            explanation.decision,
            DefaultCostModel.cse_share_decision(&candidate)
        );
        assert!(explanation.provenance.contains("structural fallback"));
    }

    #[test]
    fn decide_rejects_mixed_one_shot_and_repeating_without_horizon() {
        let subtree = scan();
        let bound = summary_node(SummaryFamilyType::ExactAggregate(
            ExactKind::Sum,
            ExactParams::Sum,
        ));
        let candidate = CseCandidate {
            subtree: &subtree,
            bound_summary: &bound,
            consumer_count: 2,
        };
        let profile = RecurrenceProfile::from_repeating_intervals(vec![interval(1000)])
            .unwrap()
            .with_one_shot_consumers(1);
        let err = decide(&DefaultCostModel, &candidate, &profile, None).unwrap_err();
        assert_eq!(err, RecurrenceError::MissingHorizon);
    }

    #[test]
    fn decide_accepts_mixed_one_shot_and_repeating_with_an_explicit_horizon() {
        let subtree = scan();
        let bound = summary_node(SummaryFamilyType::ExactAggregate(
            ExactKind::Sum,
            ExactParams::Sum,
        ));
        let candidate = CseCandidate {
            subtree: &subtree,
            bound_summary: &bound,
            consumer_count: 2,
        };
        let profile = RecurrenceProfile::from_repeating_intervals(vec![interval(1000)])
            .unwrap()
            .with_one_shot_consumers(1);
        let explanation = decide(
            &DefaultCostModel,
            &candidate,
            &profile,
            Some(Horizon(3600.0)),
        )
        .unwrap();
        assert!(explanation.maintained_total.is_some());
        assert!(explanation.recompute_total.is_some());
        assert!(explanation.summary_build_cost.is_some());
        assert_eq!(explanation.horizon, Some(Horizon(3600.0)));
    }

    // ── A deterministic cost model exercising the trait hooks directly ──

    struct DeterministicUnitCostModel;
    impl CostModel for DeterministicUnitCostModel {
        fn rank_candidates(
            &self,
            _intent: &asap_types::pre_asap::agg_intent::AggIntent,
            candidates: &[asap_types::post_asap::SketchAlgorithm],
        ) -> Vec<asap_types::post_asap::SketchAlgorithm> {
            candidates.to_vec()
        }
        fn maintenance_cost_per_update(&self, _candidate: &CseCandidate) -> Cost {
            Cost(1.0)
        }
        fn summary_read_cost(&self, _candidate: &CseCandidate) -> Cost {
            Cost(1.0)
        }
        fn raw_recompute_cost(&self, _candidate: &CseCandidate) -> Cost {
            Cost(50.0)
        }
    }

    /// Issue #287's headline acceptance criterion: with a deterministic test
    /// cost model and identical IR, a high evaluation frequency selects
    /// maintained/shared state while a sufficiently infrequent workload
    /// selects recomputation — driven purely by `evaluation_rate`, with a
    /// fixed, nonzero `update_rate` representing continuous ingest that
    /// keeps a maintained summary's floor cost independent of how often
    /// it's read.
    #[test]
    fn high_frequency_selects_maintained_low_frequency_selects_recompute() {
        let subtree = scan();
        let bound = summary_node(SummaryFamilyType::ExactAggregate(
            ExactKind::Sum,
            ExactParams::Sum,
        ));
        let candidate = CseCandidate {
            subtree: &subtree,
            bound_summary: &bound,
            consumer_count: 1,
        };

        // A steady 10Hz ingest rate underlies both scenarios — the physical
        // maintenance cost is unaffected by how often the summary is read
        // (issue #287: "repetition ... does not reduce the physical
        // maintenance work caused by ingest updates").
        let update_rate = UpdateRate(10.0);

        // High frequency: a consumer firing every 10ms => 100Hz.
        let frequent = RecurrenceProfile::from_repeating_intervals(vec![interval(10)])
            .unwrap()
            .with_update_rate(update_rate)
            .unwrap();
        let frequent_explanation =
            decide(&DeterministicUnitCostModel, &candidate, &frequent, None).unwrap();
        assert_eq!(frequent_explanation.decision, ShareDecision::Share);
        assert!(
            frequent_explanation.maintained_cost_rate.unwrap().0
                < frequent_explanation.recompute_cost_rate.unwrap().0
        );

        // Low frequency: a consumer firing every 100s => 0.01Hz.
        let infrequent = RecurrenceProfile::from_repeating_intervals(vec![interval(100_000)])
            .unwrap()
            .with_update_rate(update_rate)
            .unwrap();
        let infrequent_explanation =
            decide(&DeterministicUnitCostModel, &candidate, &infrequent, None).unwrap();
        assert_eq!(
            infrequent_explanation.decision,
            ShareDecision::RecomputeIndependently
        );
        assert!(
            infrequent_explanation.maintained_cost_rate.unwrap().0
                > infrequent_explanation.recompute_cost_rate.unwrap().0
        );
    }

    /// Update rate feeds only `maintained_cost_rate` (via
    /// `maintenance_cost_per_update`), never `recompute_cost_rate` — and
    /// evaluation rate feeds both `summary_read_cost` (maintained) and
    /// `raw_recompute_cost` (recompute), never bypassing either. Pins the
    /// issue's "update rate affects maintained-summary cost but not read
    /// frequency; evaluation rate affects summary-read and recomputation
    /// cost" acceptance criterion directly against the trait hooks.
    #[test]
    fn update_rate_only_affects_maintained_cost_evaluation_rate_affects_both() {
        let subtree = scan();
        let bound = summary_node(SummaryFamilyType::ExactAggregate(
            ExactKind::Sum,
            ExactParams::Sum,
        ));
        let candidate = CseCandidate {
            subtree: &subtree,
            bound_summary: &bound,
            consumer_count: 1,
        };

        let base = RecurrenceProfile::from_repeating_intervals(vec![interval(1000)]).unwrap();
        let with_update = base.with_update_rate(UpdateRate(1000.0)).unwrap();

        let base_explanation =
            decide(&DeterministicUnitCostModel, &candidate, &base, None).unwrap();
        let with_update_explanation =
            decide(&DeterministicUnitCostModel, &candidate, &with_update, None).unwrap();

        // Bumping update_rate alone raises maintained_cost_rate...
        assert!(
            with_update_explanation.maintained_cost_rate.unwrap().0
                > base_explanation.maintained_cost_rate.unwrap().0
        );
        // ...but leaves recompute_cost_rate (a pure function of
        // evaluation_rate, unchanged between the two profiles) untouched.
        assert_eq!(
            with_update_explanation.recompute_cost_rate,
            base_explanation.recompute_cost_rate
        );
    }

    /// One-shot consumers alone (no repeating consumer, no update rate) —
    /// still produce a real decision with no explicit `Horizon` required,
    /// by comparing the one-shot costs directly (see `decide`'s
    /// `effective_horizon` fallback): with enough one-shot consumers, the
    /// fixed `summary_build_cost` amortizes and sharing wins even though
    /// `raw_recompute_cost` is expensive.
    #[test]
    fn one_shot_only_consumer_decides_without_an_explicit_horizon() {
        let subtree = scan();
        let bound = summary_node(SummaryFamilyType::ExactAggregate(
            ExactKind::Sum,
            ExactParams::Sum,
        ));
        let candidate = CseCandidate {
            subtree: &subtree,
            bound_summary: &bound,
            consumer_count: 1,
        };
        let profile = RecurrenceProfile::EMPTY.with_one_shot_consumers(3);

        // raw_recompute_cost=50 >> summary_read_cost=1: sharing wins even
        // for purely one-shot consumers, since materializing once (paying
        // summary_build_cost=50 exactly once — DeterministicUnitCostModel's
        // default delegates build cost to raw_recompute_cost) and reading it
        // 3 times (3 * 1 = 3) totals 53, cheaper than recomputing
        // independently 3 times (50 * 3 = 150).
        let explanation = decide(&DeterministicUnitCostModel, &candidate, &profile, None).unwrap();
        assert_eq!(explanation.decision, ShareDecision::Share);
        assert_eq!(explanation.maintained_total, Some(Cost(53.0)));
        assert_eq!(explanation.recompute_total, Some(Cost(150.0)));
    }

    /// Regression for issue #287 review bug 1: without a `summary_build_cost`
    /// term, a purely one-shot comparison modeled "maintained" as free to
    /// construct, so `Share` won unconditionally for *any* number of
    /// one-shot consumers — including exactly one, where sharing can never
    /// make sense (you always pay at least as much to build-then-read once
    /// as you would to just recompute once directly). With the fix, a
    /// single one-shot consumer strictly prefers `RecomputeIndependently`.
    #[test]
    fn one_shot_only_single_consumer_does_not_unconditionally_prefer_share() {
        let subtree = scan();
        let bound = summary_node(SummaryFamilyType::ExactAggregate(
            ExactKind::Sum,
            ExactParams::Sum,
        ));
        let candidate = CseCandidate {
            subtree: &subtree,
            bound_summary: &bound,
            consumer_count: 1,
        };
        let profile = RecurrenceProfile::EMPTY.with_one_shot_consumers(1);

        let explanation = decide(&DeterministicUnitCostModel, &candidate, &profile, None).unwrap();
        // build(50) + read(1) = 51 > recompute(50) * 1 = 50.
        assert_eq!(explanation.decision, ShareDecision::RecomputeIndependently);
        assert_eq!(explanation.maintained_total, Some(Cost(51.0)));
        assert_eq!(explanation.recompute_total, Some(Cost(50.0)));
    }

    /// A batch-only workload (no `DataWorkload`, only one-shot
    /// consumers) with the *default* `DefaultCostModel` must not
    /// unconditionally prefer `Share` regardless of how many one-shot
    /// consumers there are — issue #287 review bug 1's original repro,
    /// pinned against the real default cost model rather than the test's
    /// own `DeterministicUnitCostModel`.
    #[test]
    fn batch_only_workload_does_not_unconditionally_prefer_share_under_default_cost_model() {
        let subtree = scan();
        let bound = summary_node(SummaryFamilyType::ExactAggregate(
            ExactKind::Sum,
            ExactParams::Sum,
        ));
        let candidate = CseCandidate {
            subtree: &subtree,
            bound_summary: &bound,
            consumer_count: 1,
        };
        // `DefaultCostModel`'s `raw_recompute_cost`/`summary_build_cost`
        // both delegate to the same structural-size proxy
        // (`cse_recompute_cost`), and `summary_read_cost` defaults to a
        // nominal `1.0` — build and recompute cost the same, so reading a
        // materialized copy even once more than a bare recompute can never
        // pay off: for every one-shot-only consumer count, the maintained
        // total (`build + read * n`) must be strictly greater than the
        // recompute total (`recompute * n`), i.e. `Share` must never win.
        for n in [1usize, 2, 10, 1_000] {
            let profile = RecurrenceProfile::EMPTY.with_one_shot_consumers(n);
            let explanation = decide(&DefaultCostModel, &candidate, &profile, None).unwrap();
            assert_eq!(
                explanation.decision,
                ShareDecision::RecomputeIndependently,
                "n={n}, explanation={explanation:?}"
            );
        }
    }

    // ── multiple roots sharing a sub-DAG, via PlanSpace ──────────────────

    use crate::replacement::search_workload;
    use asap_types::pre_asap::agg_intent::AggIntent;
    use asap_types::pre_asap::expr_ir::ScalarValue;
    use asap_types::pre_asap::query_expr::{Predicate, Reduction as QueryReduction};

    /// Like `scan()`, plus a "job" label column to group by — CSE's
    /// sharing legality gate requires a provable unique key
    /// (`Schema::has_unique_key`), and an *ungrouped* aggregate's empty
    /// `by` reports none (see `asap_types::pre_asap::cse`'s own "Legality"
    /// module docs); grouping by a label column gives `sum_agg()` below a
    /// real one, matching the pattern
    /// `replacement.rs`'s own CSE fixtures already use (`metric_scan`/`agg`
    /// grouped by a label column).
    fn labeled_scan() -> QueryExpr {
        QueryExpr::Scan {
            source: Source::TimeSeries { metric: "m".into() },
            predicates: vec![],
            schema: Schema::with_time_index(
                vec![
                    Column::new("ts", DataType::Timestamp, false),
                    Column::new("value", DataType::Float64, false),
                    Column::new("job", DataType::Utf8, true),
                ],
                0,
                vec![],
            ),
        }
    }

    fn sum_agg() -> QueryExpr {
        QueryExpr::Aggregate {
            reduction: QueryReduction::by(vec![2]),
            measures: vec![AggIntent::Sum { col: Some(1) }],
            output_names: vec![],
            having: None,
            child: Rc::new(labeled_scan()),
        }
    }

    /// A root wrapping a fresh, independently-built (but structurally
    /// identical to every other call's) `sum_agg()` in a `Filter` whose
    /// literal predicate is unique per root — keeps the three roots
    /// themselves structurally distinct (so they don't collapse into one
    /// root the way whole-root-identical fixtures do — see
    /// `shared_aggregate_across_two_roots_gets_both_strategies_candidates`'s
    /// own doc) while letting `share_common_subtrees` unify their
    /// identical `sum_agg()` children onto one shared `Rc`.
    fn filtered_root(distinguishing_literal: i64) -> QueryExpr {
        QueryExpr::Filter {
            pred: Predicate(Rc::new(QueryExpr::Literal(ScalarValue::Int64(
                distinguishing_literal,
            )))),
            child: Rc::new(sum_agg()),
        }
    }

    /// Three workload roots share one underlying `sum_agg()` sub-DAG: two
    /// repeating consumers with different intervals, one one-shot batch
    /// consumer. `PlanSpace::recurrence_profiles` must aggregate all three
    /// onto the shared sub-DAG's own profile: `evaluation_rate = 1/t1 +
    /// 1/t2`, `one_shot_consumers = 1` — issue #287's "support a shared
    /// sub-DAG consumed by queries with different intervals" and "multiple
    /// roots sharing a sub-DAG" acceptance criteria.
    #[test]
    fn recurrence_profiles_aggregates_mixed_intervals_across_roots_sharing_a_subdag() {
        let roots: Vec<(&str, Rc<QueryExpr>)> = vec![
            ("root_a", Rc::new(filtered_root(1))),
            ("root_b", Rc::new(filtered_root(2))),
            ("root_c", Rc::new(filtered_root(3))),
        ];
        let space = search_workload(roots);

        // Fixture sanity: the three roots stayed distinct (different
        // literal predicates), but their `sum_agg()` children merged onto
        // one shared `Rc` (consumer_count 3) — this is the "shared sub-DAG"
        // under test. Its own child Scan collapses along with it (all 3
        // Filters' aggregates now point at the *same* Aggregate Rc, so
        // there is only ever one Scan Rc underneath, directly referenced
        // from exactly one place — the shared Aggregate's own `child`).
        assert_eq!(
            space.len(),
            5,
            "3 distinct Filters + 1 shared Aggregate + 1 Scan underneath it"
        );
        let shared_group = space
            .groups()
            .find(|g| matches!(g.target.as_ref(), QueryExpr::Aggregate { .. }))
            .expect("the shared sum_agg() is a discovered target");
        assert_eq!(shared_group.consumer_count, 3, "shared by all 3 roots");

        let root_recurrence = vec![
            repeating(1_000),  // 1 Hz
            repeating(10_000), // 0.1 Hz
            RootRecurrence::OneShotCount(1),
        ];
        let profiles = space
            .recurrence_profiles(&root_recurrence, Some(UpdateRate(5.0)))
            .unwrap();

        let profile = profiles.for_target(&shared_group.target);
        let expected_rate = 1.0 / 1.0 + 1.0 / 10.0; // Hz
        assert!(
            (profile.evaluation_rate.unwrap().0 - expected_rate).abs() < 1e-9,
            "evaluation_rate={:?}",
            profile.evaluation_rate
        );
        assert_eq!(profile.one_shot_consumers, 1);
        assert_eq!(profile.update_rate, Some(UpdateRate(5.0)));

        // Each root's own unshared Filter node sees only its own
        // contribution — no cross-contamination between sibling roots: the
        // 1Hz root's own Filter carries only that 1Hz, not the combined
        // rate the shared Aggregate beneath all three carries.
        let root_a_profile = profiles.for_target(&space.roots[0].1);
        assert!(
            (root_a_profile.evaluation_rate.unwrap().0 - 1.0).abs() < 1e-9,
            "root_a's own Filter should see only its own 1Hz, not the combined rate: {:?}",
            root_a_profile.evaluation_rate
        );
        assert_eq!(root_a_profile.one_shot_consumers, 0);

        // The one-shot root's own Filter sees only its one-shot
        // contribution, no evaluation rate at all.
        let root_c_profile = profiles.for_target(&space.roots[2].1);
        assert_eq!(root_c_profile.evaluation_rate, None);
        assert_eq!(root_c_profile.one_shot_consumers, 1);
    }

    #[test]
    fn plan_selection_uses_recurrence_profiles_for_cse_choices() {
        let roots = vec![
            ("a", Rc::new(filtered_root(1))),
            ("b", Rc::new(filtered_root(2))),
        ];
        let space = search_workload(roots);
        let shared = space
            .groups()
            .find(|group| matches!(group.target.as_ref(), QueryExpr::Aggregate { .. }))
            .expect("the aggregate is shared by both roots");
        let update_rate = Some(UpdateRate(10.0));

        let frequent = space
            .recurrence_profiles(&[repeating(10), repeating(10)], update_rate)
            .unwrap();
        let infrequent = space
            .recurrence_profiles(&[repeating(100_000), repeating(100_000)], update_rate)
            .unwrap();

        let frequent_ranked = space
            .cost_sorted_with_recurrence(&DeterministicUnitCostModel, &frequent, None)
            .unwrap();
        let infrequent_ranked = space
            .cost_sorted_with_recurrence(&DeterministicUnitCostModel, &infrequent, None)
            .unwrap();
        let first_provenance = |ranked: &[crate::replacement::RankedGroup<'_>]| {
            ranked
                .iter()
                .find(|group| Rc::ptr_eq(group.target, &shared.target))
                .and_then(|group| group.candidates.first())
                .map(|candidate| candidate.provenance)
        };
        assert_eq!(
            first_provenance(&frequent_ranked),
            Some(crate::replacement::ReplacementProvenance::CseShare)
        );
        assert_eq!(
            first_provenance(&infrequent_ranked),
            Some(crate::replacement::ReplacementProvenance::CseRecompute)
        );

        let frequent_selected = space
            .global_selection_with_recurrence(&DeterministicUnitCostModel, &frequent, None)
            .unwrap();
        let infrequent_selected = space
            .global_selection_with_recurrence(&DeterministicUnitCostModel, &infrequent, None)
            .unwrap();
        assert_eq!(
            frequent_selected
                .for_target(&shared.target)
                .and_then(|group| group.chosen)
                .map(|candidate| candidate.provenance),
            Some(crate::replacement::ReplacementProvenance::CseShare)
        );
        assert_eq!(
            infrequent_selected
                .for_target(&shared.target)
                .and_then(|group| group.chosen)
                .map(|candidate| candidate.provenance),
            Some(crate::replacement::ReplacementProvenance::CseRecompute)
        );
    }

    #[test]
    fn recurrence_profiles_rejects_an_invalid_evaluation_rate() {
        let root = Rc::new(scan());
        let roots: Vec<(&str, Rc<QueryExpr>)> = vec![("only", root)];
        let space = search_workload(roots);
        let err = space
            .recurrence_profiles(&[RootRecurrence::Repeating(EvaluationRate(f64::NAN))], None)
            .unwrap_err();
        assert!(matches!(err, RecurrenceError::InvalidEvaluationRate(_)));
    }

    /// Issue #287 review bug 6: a length mismatch is a recoverable
    /// `RecurrenceError`, not a panic — `recurrence_profiles`'s whole
    /// signature promises a `Result`.
    #[test]
    fn recurrence_profiles_reports_a_root_count_mismatch_as_an_error_not_a_panic() {
        let root = Rc::new(scan());
        let roots: Vec<(&str, Rc<QueryExpr>)> = vec![("only", root)];
        let space = search_workload(roots);
        let err = space.recurrence_profiles(&[], None).unwrap_err();
        assert_eq!(
            err,
            RecurrenceError::RootCountMismatch {
                expected: 1,
                got: 0,
            }
        );
    }

    #[test]
    fn recurrence_profiles_rejects_an_invalid_update_rate() {
        let root = Rc::new(scan());
        let roots: Vec<(&str, Rc<QueryExpr>)> = vec![("only", root)];
        let space = search_workload(roots);
        let err = space
            .recurrence_profiles(
                &[RootRecurrence::OneShotCount(1)],
                Some(UpdateRate(f64::NAN)),
            )
            .unwrap_err();
        assert!(matches!(err, RecurrenceError::InvalidUpdateRate(_)));
    }

    /// Issue #287 review bug 2: a site no root's own structural tree
    /// actually reaches must not have the caller-supplied `update_rate`
    /// stamped onto it. `AvgToSumOverCountStrategy` (part of
    /// `default_strategies`, so included by `search_workload`) is a real,
    /// already-shipped source of exactly this shape: it rewrites a bare
    /// `avg` `Aggregate` into a *brand new* `Project(sum, count)` sub-DAG —
    /// `sum`/`count` are genuinely new `Rc`s, discovered via
    /// `discover_new_descendant_targets` from the *candidate's* own
    /// children, never reachable by walking the original `avg` root's own
    /// structural children (which is just the raw scan). Before the fix,
    /// this `count` site would get `{evaluation_rate: None,
    /// one_shot_consumers: 0, update_rate: Some(rate)}` — `maintained_cost_rate
    /// > 0` against a `recompute_cost_rate` of exactly `0` — unconditionally
    /// `RecomputeIndependently`, regardless of the site's own real
    /// `consumer_count`.
    #[test]
    fn recurrence_profiles_does_not_stamp_update_rate_on_a_site_unreachable_from_any_root() {
        let avg_root = QueryExpr::Aggregate {
            reduction: QueryReduction::by(vec![]),
            measures: vec![AggIntent::Avg { col: None }],
            output_names: vec![],
            having: None,
            child: Rc::new(scan()),
        };
        let roots: Vec<(&str, Rc<QueryExpr>)> = vec![("q", Rc::new(avg_root))];
        let space = search_workload(roots);

        let count_group = space
            .groups()
            .find(|g| {
                matches!(
                    g.target.as_ref(),
                    QueryExpr::Aggregate { measures, .. }
                        if measures.iter().any(|m| matches!(m, AggIntent::Count { .. }))
                )
            })
            .expect(
                "AvgToSumOverCountStrategy should have introduced a new Count aggregate \
                 target, unreachable from the original avg root's own structural children",
            );

        let root_recurrence = vec![repeating(1_000)];
        let profiles = space
            .recurrence_profiles(&root_recurrence, Some(UpdateRate(5.0)))
            .unwrap();

        let count_profile = profiles.for_target(&count_group.target);
        assert_eq!(
            count_profile,
            RecurrenceProfile::EMPTY,
            "a site unreachable from any root's own structural tree must fall back to \
             RecurrenceProfile::EMPTY (no update_rate, no evaluation_rate, no one-shot \
             consumers), not just an evaluation-rate-free profile that still carries the \
             caller's update_rate"
        );

        // The root itself (and the raw scan directly beneath it, which the
        // walk *does* reach) still get the real update_rate.
        let root_profile = profiles.for_target(&space.roots[0].1);
        assert_eq!(root_profile.update_rate, Some(UpdateRate(5.0)));
    }

    /// Issue #287 review (lower-priority item): a parent referencing the
    /// same shared child twice (`BinaryOp{lhs: X, rhs: X}`, the same shape
    /// `pre_asap::cse`'s own within-one-query sharing collapses onto one
    /// `Rc`) must credit that child with 2 contributions per repeating
    /// root, matching how `MemoGroup::consumer_count` already counts that
    /// exact structural occurrence twice — not 1, which a plain
    /// reachability-set walk would (wrongly) collapse it to.
    #[test]
    fn recurrence_profiles_credits_a_direct_repeated_reference_by_its_multiplicity() {
        let root = QueryExpr::BinaryOp {
            op: asap_types::pre_asap::query_expr::BinaryOpKind::Compare(
                asap_types::pre_asap::expr_ir::CompareOpKind::Eq,
            ),
            lhs: Rc::new(sum_agg()),
            rhs: Rc::new(sum_agg()),
            vector_match: None,
        };
        let space = search_workload(vec![("q", Rc::new(root))]);

        let shared_group = space
            .groups()
            .find(|g| matches!(g.target.as_ref(), QueryExpr::Aggregate { .. }))
            .expect("sum_agg() should merge onto one shared Rc, referenced twice from BinaryOp");
        assert_eq!(
            shared_group.consumer_count, 2,
            "fixture sanity: referenced twice from the same BinaryOp parent"
        );

        let root_recurrence = vec![repeating(1_000)]; // 1 Hz
        let profiles = space.recurrence_profiles(&root_recurrence, None).unwrap();
        let profile = profiles.for_target(&shared_group.target);

        // Referenced twice from the one root: evaluation_rate should be
        // 2 * 1Hz = 2Hz, matching consumer_count's own multiplicity — not
        // 1Hz, which would undercount by treating "reachable at all" as
        // the whole story.
        assert!(
            (profile.evaluation_rate.unwrap().0 - 2.0).abs() < 1e-9,
            "evaluation_rate={:?}",
            profile.evaluation_rate
        );

        let scan_group = space
            .groups()
            .find(|group| matches!(group.target.as_ref(), QueryExpr::Scan { .. }))
            .expect("the shared aggregate has a scan descendant");
        assert_eq!(
            profiles
                .for_target(&scan_group.target)
                .evaluation_rate
                .unwrap(),
            EvaluationRate(2.0),
            "ancestor multiplicity must propagate transitively to descendants"
        );
    }

    // ── Horizon validation ────────────────────────────────────────────────

    #[test]
    fn decide_rejects_a_zero_or_negative_horizon() {
        let subtree = scan();
        let bound = summary_node(SummaryFamilyType::ExactAggregate(
            ExactKind::Sum,
            ExactParams::Sum,
        ));
        let candidate = CseCandidate {
            subtree: &subtree,
            bound_summary: &bound,
            consumer_count: 2,
        };
        let profile = RecurrenceProfile::from_repeating_intervals(vec![interval(1000)])
            .unwrap()
            .with_one_shot_consumers(1);
        for bad in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            let err =
                decide(&DefaultCostModel, &candidate, &profile, Some(Horizon(bad))).unwrap_err();
            assert!(
                matches!(err, RecurrenceError::InvalidHorizon(_)),
                "bad={bad}, err={err:?}"
            );
        }
    }
}
