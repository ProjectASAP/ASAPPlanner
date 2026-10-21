//! Stage 1 (logical ASAP-aware optimization), Pass 1: per-computation
//! candidate generation, as defined in
//! `docs/design_docs/proposals/planner-layering.md`.
//!
//! [`generate_per_computation_candidates`] takes the frontends' logical DAGs
//! (`QueryRoot`s) and returns, for every eligible computation, its unranked
//! candidates. A computation ([`ComputationTarget`]) is one measure of an
//! `Aggregate` whose intent has a realization besides plain exact execution;
//! nested aggregates and operators read by scalar roots are found too.
//! Pass 1 does not consider sharing across computations; that is Pass 2.
//!
//! - A candidate says how a target is computed: exact or a summary
//!   ([`ComputationCandidate`]). Each target has exactly one exact option:
//!   its exact accumulator states (`ExactAccumulator`; `[Sum, Count]` for
//!   `avg`) when they can be built, otherwise plain `Exact`. Whether they are
//!   materialized is decided in Stage 2.
//! - A computation that fuses a target with the target beneath it is the
//!   outer target's [`FusedComputation`], which absorbs the ones beneath and
//!   has its own candidates: the whole-expression top-k over a per-item
//!   `sum` or `count` (`Summary` of a heap family) and the nested sum, a
//!   grouped `sum` over a `sum` (`ExactAccumulator([Sum])`).
//! - Summary candidates are unsized; they are sized after Pass 2 sharing.
//! - Candidates are pruned only when provably unable to meet the accuracy
//!   requirement or to run, or when the exact candidate provably dominates
//!   them (a PromQL top-k's heap over its instant vector, one row per item:
//!   [`PruneReason::TopKHeapOverOneRowPerItem`]); each pruned candidate
//!   keeps its [`PruneReason`].
//! - A measure filter or `HAVING` prunes nothing. The target records the
//!   measure filter (applied as `SummaryAgg.filter`) and one [`HavingEntry`]
//!   per consumer `Filter` directly above the aggregate: a key-only predicate
//!   applied by every consumer may go below the aggregate, anything else stays
//!   above the output (approximate outputs then get guarantee `None`).
//!
//! The input must be a logical DAG: ASAP operators, execution timing or a
//! materialization are rejected. No cost or accuracy model is used; ranking
//! and discarding valid candidates belong to Stage 3.
//!
//! # Whole-workload candidates (logical ASAP-aware optimization, end of Pass 1)
//!
//! [`compose_logical_candidate`] is Algorithm 1's
//! `SUBSTITUTE(d, s_1 ↦ c_1, …)`: it picks one per-computation candidate per
//! target (`choice[i]` indexes `targets[i]`'s candidates, then its fused
//! computation's, [`ComputationTarget::computation_for`]) and builds one whole-workload
//! candidate DAG. An aggregate whose chosen candidates are all `Exact` is
//! kept. Otherwise each of its measures is substituted on its own over the
//! aggregate's input:
//!
//! - `ExactAccumulator` with one state: `SummaryAgg` with the exact family →
//!   `FinalizeExactAccumulator`.
//! - `ExactAccumulator([Sum, Count])` of an `avg`: the finalized `sum` and
//!   `count` accumulators divided: a PromQL `BinaryOp`, or for SQL rows a
//!   join on the group keys (NULL keys match) and a SQL division, NULL where
//!   the count is 0.
//! - `Summary`: `SummaryAgg` → `SummaryEstimate`.
//! - `Exact` (or a measure that is no target): a single-measure `Aggregate`.
//!
//! A fused computation's candidate reads the absorbed target's input instead:
//! the whole-expression top-k is one heap `SummaryAgg` keyed by the ranked
//! item → `SummaryEstimate`; the nested sum is `SummaryAgg` with the exact
//! `Sum` family grouped by the outer keys → `FinalizeExactAccumulator`.
//!
//! The substituted output keeps the aggregate's column names. The measures
//! of a multi-measure aggregate are joined back on the group keys (NULL keys
//! match, as grouping treats them). A measure filter becomes
//! `SummaryAgg.filter`, so a group with no matching row stays. A
//! `BelowAggregate` `HAVING` becomes a `Filter` below the state, with its key
//! columns renumbered to the input's, and the consumer `Filter` it came from
//! is dropped; it is not a `SummaryAgg.filter`, which would keep the dropped
//! groups. An `AboveOutput` `HAVING` stays a `Filter` above the output.
//!
//! Summaries are sized here, each for its own measure's accuracy requirement.
//! When a query's value composes several approximate inputs (every sketch
//! summary below its root, when its guarantee is not exact), each is sized
//! for an equal share of that requirement: `ε/n` (a relative error
//! `(1 + ε)^{1/n} − 1`) and `δ/n` for `n` inputs (#669b). Only the inputs
//! approximate in this candidate count. Stage 1 sizes for ε and δ per output
//! value; plan selection enlarges the parameters that depend on δ for a
//! target over the whole result. Sizing belongs after Pass 2, where a shared
//! summary takes the strictest requirement among its consumers; Pass 2
//! sharing is not implemented yet, so this is that post-Pass-2 sizing step
//! with no sharing.
//!
//! Every node of the candidate gets a theoretical guarantee, composed with
//! the built-in accuracy model through operators (#669): exact over exact
//! inputs; a `SummaryEstimate` the sized family's bound where that bound
//! answers the estimate (not for Top-K, where a heap sketch bounds item
//! frequencies, not membership; not for a Count-Min family over weights not
//! proven non-negative); aggregation and arithmetic over approximate values
//! by their composition rules, with statistics left symbolic for plan
//! selection to bind; and `None` where no rule applies (comparisons and
//! value predicates, rank errors). Plan selection's accuracy model decides
//! a candidate without one.
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use asap_types::ir::operator::operator_properties::{BinaryOpKind, JoinKind, Reduction, Source};
use asap_types::ir::operator::pane_chain::{merged_chain, whole_window_build};
use asap_types::ir::operator::AggIntent;
use asap_types::ir::properties::guarantee::{
    AccuracyGuarantee, AccuracyMetric, BoundExpr, CompositionOperator, GuaranteeSource,
    ProbabilityExpr,
};
use asap_types::ir::properties::AccuracyTarget;
use asap_types::ir::scalar::expr_ir::ArithmeticOpKind;
use asap_types::ir::scalar::{resolve_column_ref, ColumnRef, CompareOpKind, ScalarValue};
use asap_types::ir::schema::{DataType, Schema};
use asap_types::ir::schema::{
    EntityIdentity, ExactKind, ExactParams, FamilyCapability, FieldDataType, GroupingStrategy,
    MergedError, NonNegativeWeightProof, SketchAlgorithm, SketchKind, SketchParams,
    SketchStatistic, SummaryInputExpr, SummaryUpdate, WeightDomain,
};
use asap_types::ir::{
    ASAPOp, BinaryOperator, ExprSemantics, NonASAPOp, Operator, OperatorNode, OperatorResultKind,
    Predicate, ProjectItem, QueryRoot, ScalarExpr, SchemaDerivationError,
};
use asap_types::physical::Materialization;
use thiserror::Error;

use crate::accuracy::composition::propagate;
use crate::accuracy::composition::raw_data_samples_sign_statistic;
use crate::accuracy::composition::{
    additive_as_absolute, StreamNorm, DIVISION_OPERAND_DOMAINS, GROUP_ROW_COUNT,
    PRODUCT_OPERAND_DOMAINS, QUOTIENT_OPERAND_DOMAINS,
};
use crate::accuracy::estimators::local_guarantee;
use crate::accuracy::estimators::{hll, univmon};
use crate::accuracy::{per_input_target, PropagationStats};
use crate::deployment_capabilities::DeploymentCapabilities;
use crate::function_rules::approximate_input_composition;
use crate::realization::{accuracy_budget, default_size_params, sizing_target, summary_candidates};

/// How one Pass 1 target is computed: exact or a summary.
///
/// Each target has exactly one exact option: `ExactAccumulator` when the
/// intent has an exact mergeable state and it can be built, otherwise
/// `Exact`. The plain form would duplicate "accumulator, not materialized";
/// Stage 2/3 decide materialization and sharing. Which computation is
/// computed is the target's ([`ComputationTarget`]) or its fused
/// computation's ([`FusedComputation`]).
#[derive(Debug, Clone, PartialEq)]
pub enum ComputationCandidate {
    /// Plain exact execution of the original sub-DAG; the exact option only
    /// when the intent has no exact accumulator form or that form is pruned.
    Exact,
    /// One or more exact mergeable accumulator states, by accumulator kind,
    /// finalized and combined into the measure: one state for `sum`,
    /// `count`, `min`, `max`, `rate`, `irate` and `increase`; `[Sum, Count]`,
    /// divided, for `avg`.
    ExactAccumulator(Vec<ExactKind>),
    /// An unsized summary family, sized later for the measure's accuracy.
    Summary(SketchAlgorithm),
}

/// `FieldDataType::ExactAggregate` of accumulator `kind`, with its parameters
/// (none has any, so they mirror `kind`).
fn exact_family(kind: ExactKind) -> FieldDataType {
    let params = match kind {
        ExactKind::Sum => ExactParams::Sum,
        ExactKind::Count => ExactParams::Count,
        ExactKind::Min => ExactParams::Min,
        ExactKind::Max => ExactParams::Max,
        ExactKind::Rate => ExactParams::Rate,
        ExactKind::IRate => ExactParams::IRate,
        ExactKind::Increase => ExactParams::Increase,
    };
    FieldDataType::ExactAggregate(kind, params)
}

/// Why Pass 1 proved a candidate unable to realize its target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PruneReason {
    /// The measure requires an exact answer and the summary is approximate.
    RequiresExact,
    /// `count` counts rows, so a possibly-null `avg` input changes the result.
    NullableAvgInput,
    /// The deployment's executor cannot build the summary's family or
    /// answer the measure's readout from it
    /// ([`DeploymentCapabilities::supports`]).
    UnsupportedByDeployment,
    /// The quantile's φ is outside [0, 1], where PromQL answers -Inf or
    /// +Inf: no sketch estimates that, and the executor's sketches refuse
    /// such a φ, so only the exact answer remains.
    QuantileOutsideUnitInterval,
    /// A PromQL top-k's heap (Count-Min or CountSketch with a heap) over its
    /// own input, an instant vector with one row per ranked series or
    /// group: the heap reads every row and adds
    /// nothing to it, so an exact top-k over the same rows, which keeps only
    /// `k` of them, is exact and no costlier. Only the whole-expression heap
    /// over the inner aggregate's input, where an item's rows repeat, is
    /// offered ([`FusedComputation`]).
    TopKHeapOverOneRowPerItem,
}

/// Unranked candidates of one measure and the candidates pruned for it.
#[derive(Debug, Clone, PartialEq)]
pub struct ComputationChoices {
    pub candidates: Vec<ComputationCandidate>,
    pub pruned: Vec<(ComputationCandidate, PruneReason)>,
}

impl ComputationChoices {
    /// Prune matching candidates; a pruned exact accumulator form is replaced by
    /// plain `Exact`, so the target keeps one exact option.
    fn prune(&mut self, reason: PruneReason, applies: impl Fn(&ComputationCandidate) -> bool) {
        let (pruned, kept): (Vec<_>, _) = std::mem::take(&mut self.candidates)
            .into_iter()
            .partition(|choice| *choice != ComputationCandidate::Exact && applies(choice));
        self.candidates = kept;
        if pruned
            .iter()
            .any(|choice| matches!(choice, ComputationCandidate::ExactAccumulator(_)))
        {
            self.candidates.insert(0, ComputationCandidate::Exact);
        }
        self.pruned.extend(
            pruned
                .into_iter()
                .map(|choice: ComputationCandidate| (choice, reason)),
        );
    }
}

/// Where a target's `HAVING` is applied. Composition (#575) applies it.
///
/// Only the adjacent form is recorded for push-down: the aggregate's own
/// `having`, or a `Filter` directly above the aggregate (the form the SQL
/// frontend lowers `HAVING` to). A value predicate anywhere above an
/// approximate output gets `guarantee = None` through guarantee propagation
/// (#575); multi-level push-down is tracked in #664.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HavingPlacement {
    /// The predicate reads only group keys and every consumer of the
    /// aggregate applies it, so it is pushed below the aggregate as a row
    /// predicate (a `Filter` on its input, which drops the group).
    BelowAggregate,
    /// The predicate reads aggregate values, so it stays a `Filter` above the
    /// finalize/estimate output; approximate candidates of this target carry
    /// `guarantee = None`, since membership near the threshold may change.
    AboveOutput,
}

/// One `HAVING` over a target's aggregate output.
#[derive(Debug, Clone, PartialEq)]
pub struct HavingEntry {
    /// The consumer `Filter` that applies it; `None` for the aggregate's own
    /// `having`, which every consumer reads.
    pub filter: Option<Rc<OperatorNode>>,
    pub predicate: Predicate,
    pub placement: HavingPlacement,
}

/// One measure of an aggregate with at least one realization besides exact
/// execution. The target retains source, grouping, filters, input
/// expressions and evaluation context.
#[derive(Debug, Clone)]
pub struct ComputationTarget {
    /// The `Aggregate` node the measure belongs to.
    pub aggregate: Rc<OperatorNode>,
    /// Index into the aggregate's measures.
    pub measure: usize,
    /// The measure's unranked candidates, the exact option first, and the
    /// candidates Pass 1 pruned with their reasons.
    pub choices: ComputationChoices,
    /// The measure's own filter; only rows that pass it update the state
    /// (`SummaryAgg.filter`).
    pub filter: Option<Predicate>,
    /// The aggregate's own `having`, then one entry per consumer `Filter`
    /// directly above the aggregate.
    pub having: Vec<HavingEntry>,
    /// The fused computation of this measure together with the target
    /// directly beneath it, offered beside this target's own candidates;
    /// `None` when no fusion applies.
    pub fused: Option<FusedComputation>,
    /// The metric whose raw data samples, or plain sums of them, are every value
    /// the aggregate reads ([`raw_data_samples_of_metric`]); `None` otherwise. A
    /// heap-sketch weight read from them carries the open requirement
    /// [`WeightDomain::RawDataSamplesOfMetric`], which plan selection confirms.
    pub raw_data_samples_of: Option<String>,
}

/// A target's measure computed together with the targets beneath it, which
/// it absorbs (they are not computed): the whole-expression top-k over a
/// per-item `sum` or `count` (`Summary` of a heap family), or the nested
/// sum, a grouped `sum` over a `sum` (`ExactAccumulator([Sum])`). It has the
/// outer target's aggregate, measure, filter and `HAVING`.
#[derive(Debug, Clone, PartialEq)]
pub struct FusedComputation {
    /// The indices in [`PerComputationCandidates::targets`] of the targets
    /// it absorbs: the one directly beneath, then, for a whole-expression
    /// heap over the raw samples beneath a grouped sum of per-series sums or
    /// counts (`topk(k, sum by (G) (sum_over_time(m[W])))`), the per-series
    /// target beneath that.
    pub absorbs: Vec<usize>,
    /// How the fused computation is computed: exact or a summary. Empty
    /// when Pass 1 pruned every one ([`Self::pruned`]); the fusion is then
    /// not offered.
    pub candidates: Vec<ComputationCandidate>,
    /// The candidates Pass 1 pruned from the fused computation, with their
    /// reasons (today only [`PruneReason::UnsupportedByDeployment`]); empty
    /// by default.
    pub pruned: Vec<(ComputationCandidate, PruneReason)>,
}

impl ComputationTarget {
    /// How many values this target's digit of a whole-workload choice
    /// takes: its own candidates, then its fused computation's.
    pub fn choice_count(&self) -> usize {
        self.choices.candidates.len()
            + self
                .fused
                .as_ref()
                .map_or(0, |fused| fused.candidates.len())
    }

    /// How the measure is computed when this target's digit of a
    /// whole-workload choice is `index`: the candidate, and the target it
    /// absorbs when the candidate is the fused computation's (past this
    /// target's own candidates). `None` past [`Self::choice_count`].
    pub fn computation_for(
        &self,
        index: usize,
    ) -> Option<(&ComputationCandidate, Option<&[usize]>)> {
        let own = self.choices.candidates.len();
        match self.choices.candidates.get(index) {
            Some(candidate) => Some((candidate, None)),
            None => {
                let fused = self.fused.as_ref()?;
                Some((
                    fused.candidates.get(index - own)?,
                    Some(fused.absorbs.as_slice()),
                ))
            }
        }
    }
}

/// Compact Pass 1 inventory; roots and nested producer dependencies are retained.
#[derive(Debug, Clone)]
pub struct PerComputationCandidates<Id> {
    /// The frontends' query roots with their ids, in workload order; whole-
    /// workload candidates are these roots with chosen candidates substituted.
    pub roots: Vec<(Id, QueryRoot)>,
    /// The targets; a whole-workload choice has one digit per target,
    /// ranging over [`ComputationTarget::choice_count`].
    pub targets: Vec<ComputationTarget>,
}

impl<Id> PerComputationCandidates<Id> {
    /// The number of whole-workload candidates: the size of the Cartesian
    /// product of the targets' candidates, where a target with a fused
    /// computation and the targets beneath that it absorbs count as their
    /// own combinations plus the fused computation's candidates (the
    /// absorbed targets are not computed then), saturating at `usize::MAX`.
    /// A target with `n` candidates and `f` fused ones counts `n · Π m + f`,
    /// the product over the targets it absorbs that no other of them
    /// absorbs, each counted the same way. It is exactly the number of
    /// choices enumeration keeps ([`Self::absorbed_choice_varies`]).
    pub fn combinations(&self) -> usize {
        let absorbed: HashSet<usize> = absorbed_targets(&self.targets).collect();
        (0..self.targets.len())
            .filter(|index| !absorbed.contains(index))
            .map(|index| self.combinations_of(index))
            .fold(1usize, usize::saturating_mul)
    }

    /// [`Self::combinations`] of target `index` with the targets it absorbs.
    fn combinations_of(&self, index: usize) -> usize {
        let target = &self.targets[index];
        let n = target.choices.candidates.len();
        let Some(fused) = &target.fused else {
            return n;
        };
        let absorbed_by_another = |a: usize| {
            fused.absorbs.iter().any(|&b| {
                self.targets[b]
                    .fused
                    .as_ref()
                    .is_some_and(|inner| inner.absorbs.contains(&a))
            })
        };
        fused
            .absorbs
            .iter()
            .filter(|&&a| !absorbed_by_another(a))
            .map(|&a| self.combinations_of(a))
            .fold(n, usize::saturating_mul)
            .saturating_add(fused.candidates.len())
    }

    /// Whether `choice` picks a fused computation and a candidate other
    /// than the first for a target it absorbs: the same whole-workload
    /// candidate as the one with the first, so enumeration skips it.
    pub fn absorbed_choice_varies(&self, choice: &[usize]) -> bool {
        self.targets.iter().zip(choice).any(|(target, &index)| {
            target
                .computation_for(index)
                .and_then(|(_, absorbed)| absorbed)
                .is_some_and(|inner| inner.iter().any(|&i| choice[i] != 0))
        })
    }

    /// This inventory without the summary candidates `deployment` does not
    /// support: a target's own are moved to its pruned candidates with
    /// [`PruneReason::UnsupportedByDeployment`], and so are a fused
    /// computation's (with all of them pruned, the fusion is not offered).
    /// The readout is the
    /// statistic the measure's estimate reads (the outer measure's for a
    /// fused computation). With every summary supported it is unchanged.
    pub fn without_unsupported_summaries(mut self, deployment: &DeploymentCapabilities) -> Self {
        for target in &mut self.targets {
            let Some(NonASAPOp::Aggregate { measures, .. }) = target.aggregate.non_asap() else {
                continue;
            };
            let Ok(readout) = statistic(&measures[target.measure]).map(|s| s.kind()) else {
                continue;
            };
            let unsupported = |candidate: &ComputationCandidate| {
                matches!(candidate, ComputationCandidate::Summary(family)
                    if !deployment.supports(family, readout))
            };
            target
                .choices
                .prune(PruneReason::UnsupportedByDeployment, unsupported);
            if let Some(fused) = &mut target.fused {
                let (pruned, kept): (Vec<_>, _) = std::mem::take(&mut fused.candidates)
                    .into_iter()
                    .partition(unsupported);
                fused.candidates = kept;
                fused.pruned.extend(
                    pruned
                        .into_iter()
                        .map(|candidate| (candidate, PruneReason::UnsupportedByDeployment)),
                );
            }
        }
        self
    }
}

/// The indices of `targets` some target's fused computation absorbs.
fn absorbed_targets(targets: &[ComputationTarget]) -> impl Iterator<Item = usize> + '_ {
    targets
        .iter()
        .filter_map(|target| target.fused.as_ref())
        .flat_map(|fused| fused.absorbs.iter().copied())
}

#[derive(Debug, Error)]
pub enum PerComputationCandidateError {
    #[error(transparent)]
    Structure(#[from] SchemaDerivationError),
    #[error("logical candidate input already has assigned execution timing or materialization")]
    AssignedPhysicalAnnotation,
    #[error("logical candidate input already contains ASAP operators")]
    ContainsAsap,
    /// `root` (the root id's `Debug` form) and `measure` locate the target;
    /// [`computation_candidates_for_intent`] alone leaves them `None`.
    #[error(
        "approximate accuracy of root {}, measure {} requires epsilon and delta in (0, 1)",
        root.as_deref().unwrap_or("?"),
        measure.map_or_else(|| "?".to_string(), |measure| measure.to_string())
    )]
    InvalidAccuracy {
        root: Option<String>,
        measure: Option<usize>,
    },
    #[error("choice must name one listed candidate per target")]
    InvalidChoice,
    #[error("unsupported substitution in logical ASAP-aware optimization: {0}")]
    Unsupported(&'static str),
    /// A Pass 2 rule found more than `max` candidates of one kind, whose
    /// 2^n subsets it would enumerate: the workload is too large to plan
    /// this way, not bad input.
    #[error("found {found} {counted}, more than {max}")]
    TooManyCandidates {
        /// What was counted, e.g. "rollup candidates".
        counted: &'static str,
        /// How many were found, more than `max`.
        found: usize,
        /// The cap that was exceeded.
        max: usize,
    },
}

/// Enumerate exact, rewrite and summary choices of one measure in stable catalog
/// order, without ranking or empirical assessment. A summary is not a claim that
/// a deployment meets the measure's accuracy requirement. The one exact option
/// comes first: the exact accumulator form if the intent has one, else `Exact`.
pub fn computation_candidates_for_intent(
    intent: &AggIntent,
) -> Result<ComputationChoices, PerComputationCandidateError> {
    let states = match intent {
        AggIntent::Count { .. } => vec![ExactKind::Count],
        AggIntent::Sum { .. } => vec![ExactKind::Sum],
        AggIntent::Min { .. } => vec![ExactKind::Min],
        AggIntent::Max { .. } => vec![ExactKind::Max],
        AggIntent::Rate => vec![ExactKind::Rate],
        AggIntent::IRate => vec![ExactKind::IRate],
        AggIntent::Increase => vec![ExactKind::Increase],
        AggIntent::Avg { .. } => vec![ExactKind::Sum, ExactKind::Count],
        _ => vec![],
    };
    let exact = if states.is_empty() {
        ComputationCandidate::Exact
    } else {
        ComputationCandidate::ExactAccumulator(states)
    };
    let mut choices = ComputationChoices {
        candidates: vec![exact],
        pruned: vec![],
    };
    if let Some(target) = sizing_target(intent) {
        if !target.is_exact() {
            let (epsilon, delta) = accuracy_budget(&target);
            let unit = |value: f64| value > 0.0 && value < 1.0;
            if !unit(epsilon) || !unit(delta) {
                return Err(PerComputationCandidateError::InvalidAccuracy {
                    root: None,
                    measure: None,
                });
            }
        }
        let summaries = summary_candidates(intent).iter().cloned();
        if matches!(intent, AggIntent::Quantile { q, .. } if !(0.0..=1.0).contains(q)) {
            choices.pruned.extend(summaries.map(|algorithm| {
                (
                    ComputationCandidate::Summary(algorithm),
                    PruneReason::QuantileOutsideUnitInterval,
                )
            }));
        } else if target.is_exact() {
            choices.pruned.extend(summaries.map(|algorithm| {
                (
                    ComputationCandidate::Summary(algorithm),
                    PruneReason::RequiresExact,
                )
            }));
        } else {
            choices
                .candidates
                .extend(summaries.map(ComputationCandidate::Summary));
        }
    }
    Ok(choices)
}

/// Whether an `avg` measure's input may be null, so `count` rows differ from
/// counted values. An unresolvable input is treated as possibly null.
fn nullable_avg_input(node: &OperatorNode, intent: &AggIntent) -> bool {
    let Some(NonASAPOp::Aggregate { child, .. }) = node.non_asap() else {
        return true;
    };
    let column = match intent.input_cols().first() {
        Some(column) => Some(*column),
        None => resolve_column_ref(&ColumnRef::SampleValue, &child.schema).ok(),
    };
    column
        .and_then(|column| child.schema.fields.get(column))
        .is_none_or(|field| field.nullable)
}

/// Whether `having` over `aggregate`'s output reads only `by(...)` group keys.
/// A global aggregate (no group keys) never qualifies: its one output row
/// exists even when no input row does, so no row predicate can stand in.
fn key_only_having(aggregate: &OperatorNode, having: &Predicate) -> bool {
    let keys = match aggregate.non_asap() {
        Some(NonASAPOp::Aggregate {
            reduction: Reduction::Reduce(by),
            ..
        }) if !by.is_without() => by.len(),
        _ => 0,
    };
    keys > 0
        && having.0.operator_refs().is_empty()
        && having.0.columns_referenced().iter().all(|id| *id < keys)
}

fn placement(below: bool) -> HavingPlacement {
    if below {
        HavingPlacement::BelowAggregate
    } else {
        HavingPlacement::AboveOutput
    }
}

/// Discover one target per measure of each eligible aggregate, including
/// operator plans read by scalar roots. An aggregate measure is eligible when it
/// has a realization besides exact execution, even if every such realization
/// is pruned; the prune reasons are reported on the target. Measure filters
/// and `HAVING` keep every candidate; the target records how they are applied.
/// A consumer `Filter`'s key-only `HAVING` is pushed below only when every
/// consumer of the aggregate applies the same predicate.
pub fn generate_per_computation_candidates<Id: std::fmt::Debug>(
    roots: Vec<(Id, QueryRoot)>,
) -> Result<PerComputationCandidates<Id>, PerComputationCandidateError> {
    let mut seen = HashSet::new();
    // Each aggregate with the first root that reaches it.
    let mut aggregates = Vec::new();
    // Every consumer of each node: `Some(parent)`, or `None` for a root or
    // scalar reference.
    let mut consumers: HashMap<*const OperatorNode, Vec<Option<Rc<OperatorNode>>>> = HashMap::new();
    for (id, root) in &roots {
        root.validate_structure()?;
        let operators = match root {
            QueryRoot::Operator(node) => vec![node],
            QueryRoot::Scalar(expr) => expr.operator_refs(),
        };
        for root in operators {
            if root.contains_asap() {
                return Err(PerComputationCandidateError::ContainsAsap);
            }
            consumers.entry(Rc::as_ptr(root)).or_default().push(None);
            for node in OperatorNode::reachable(root) {
                if !seen.insert(Rc::as_ptr(&node)) {
                    continue;
                }
                if node.timing.is_some() || node.materialization != Materialization::NotMaterialized
                {
                    return Err(PerComputationCandidateError::AssignedPhysicalAnnotation);
                }
                for child in node.children() {
                    consumers
                        .entry(Rc::as_ptr(child))
                        .or_default()
                        .push(Some(Rc::clone(&node)));
                }
                if matches!(node.non_asap(), Some(NonASAPOp::Aggregate { .. })) {
                    aggregates.push((format!("{id:?}"), node));
                }
            }
        }
    }
    let mut targets = Vec::new();
    for (root, node) in aggregates {
        let Some(NonASAPOp::Aggregate {
            measures,
            filters,
            having,
            ..
        }) = node.non_asap()
        else {
            continue;
        };
        let mut entries: Vec<HavingEntry> = having
            .iter()
            .map(|having| HavingEntry {
                filter: None,
                predicate: having.clone(),
                placement: placement(key_only_having(&node, having)),
            })
            .collect();
        let parents = &consumers[&Rc::as_ptr(&node)];
        // Only a `Filter` whose input is this aggregate is its `HAVING`; one
        // that reads it from a scalar expression stays a plain consumer.
        let mut filters_above: Vec<(&Rc<OperatorNode>, &Predicate)> = Vec::new();
        for parent in parents.iter().flatten() {
            if let Some(NonASAPOp::Filter { child, pred }) = parent.non_asap() {
                if Rc::ptr_eq(child, &node)
                    && !filters_above
                        .iter()
                        .any(|(filter, _)| Rc::ptr_eq(filter, parent))
                {
                    filters_above.push((parent, pred));
                }
            }
        }
        for (filter, predicate) in &filters_above {
            let shared_by_all = filters_above.len() == parents.len()
                && filters_above.iter().all(|(_, other)| other == predicate);
            entries.push(HavingEntry {
                filter: Some(Rc::clone(filter)),
                predicate: (*predicate).clone(),
                placement: placement(shared_by_all && key_only_having(&node, predicate)),
            });
        }
        for (measure, intent) in measures.iter().enumerate() {
            let mut choices =
                computation_candidates_for_intent(intent).map_err(|error| match error {
                    PerComputationCandidateError::InvalidAccuracy { .. } => {
                        PerComputationCandidateError::InvalidAccuracy {
                            root: Some(root.clone()),
                            measure: Some(measure),
                        }
                    }
                    error => error,
                })?;
            if choices.candidates == [ComputationCandidate::Exact] && choices.pruned.is_empty() {
                continue;
            }
            if matches!(intent, AggIntent::Avg { .. }) && nullable_avg_input(&node, intent) {
                choices.prune(PruneReason::NullableAvgInput, |choice| {
                    matches!(choice, ComputationCandidate::ExactAccumulator(_))
                });
            }
            targets.push(ComputationTarget {
                aggregate: node.clone(),
                measure,
                choices,
                filter: filters.get(measure).cloned().flatten(),
                having: entries.clone(),
                fused: None,
                raw_data_samples_of: node.non_asap().and_then(|op| match op {
                    NonASAPOp::Aggregate { child, .. } => raw_data_samples_of_metric(child),
                    _ => None,
                }),
            });
        }
    }
    add_whole_expression_targets(&mut targets, &consumers);
    prune_topk_heaps_over_one_row_per_item(&mut targets);
    add_nested_sum_targets(&mut targets, &consumers);
    Ok(PerComputationCandidates { roots, targets })
}

/// Nested-sum merge: a grouped `sum` over the value of a `sum` target
/// beneath it gets a fused computation ([`ComputationTarget::fused`])
/// with the one candidate `ExactAccumulator([Sum])`: one exact `Sum`
/// accumulator over the inner sum's input, grouped by the outer keys (the
/// sum of the inner groups' sums is the sum of their rows), absorbing the
/// inner target. Applied only when the inner sum has no other consumer, so
/// absorbing it removes its work, when neither has a filter or `HAVING`
/// ([`nested_sum_input`]), and when the outer sum is not itself absorbed by
/// a merge and has no fused computation yet, and the inner sum has none, so
/// merges do not stack. An outer sum a whole-expression top-k absorbs
/// (`topk(k, sum by (G) (sum_over_time(m[W])))`) keeps its merge for the
/// top-k's own candidates.
fn add_nested_sum_targets(
    targets: &mut [ComputationTarget],
    consumers: &HashMap<*const OperatorNode, Vec<Option<Rc<OperatorNode>>>>,
) {
    // The targets a merge absorbs.
    let mut absorbed: HashSet<usize> = HashSet::new();
    for t in 0..targets.len() {
        let target = &targets[t];
        if absorbed.contains(&t) || target.fused.is_some() || !target.having.is_empty() {
            continue;
        }
        let Some(NonASAPOp::Aggregate { child, .. }) = target.aggregate.non_asap() else {
            continue;
        };
        let Some(inner) = targets
            .iter()
            .position(|other| Rc::ptr_eq(&other.aggregate, child))
        else {
            continue;
        };
        // A target with a fused computation is never absorbed: the inner sum
        // must not have one of its own.
        if consumers.get(&Rc::as_ptr(child)).map(Vec::len) != Some(1)
            || !targets[inner].having.is_empty()
            || targets[inner].fused.is_some()
            || nested_sum_input(&target.aggregate).is_none()
        {
            continue;
        }
        absorbed.insert(inner);
        let merge = ComputationCandidate::ExactAccumulator(vec![ExactKind::Sum]);
        targets[t].fused = Some(FusedComputation {
            absorbs: vec![inner],
            candidates: vec![merge],
            pruned: Vec::new(),
        });
    }
}

/// The input rows, the inner sum and the outer grouping (moved onto the
/// input rows by column, [`partitions_over`]) of a nested-sum merge of
/// `target`, or `None` when it does not apply: `target` is a grouped `sum`
/// (not by `without`) of the value column of a per-series or grouped `sum`
/// directly beneath it, neither filtered nor with `HAVING`, and not grouped
/// by the metric name a per-series inner sum drops.
fn nested_sum_input(target: &OperatorNode) -> Option<(Rc<OperatorNode>, AggIntent, Reduction)> {
    use asap_types::ir::operator::non_asap::any_measure_filtered;
    let Some(NonASAPOp::Aggregate {
        child: inner,
        reduction: reduction @ Reduction::Reduce(by),
        measures,
        filters,
        having: None,
        ..
    }) = target.non_asap()
    else {
        return None;
    };
    let [AggIntent::Sum { col }] = measures.as_slice() else {
        return None;
    };
    let Some(NonASAPOp::Aggregate {
        child: rows,
        reduction: inner_reduction,
        measures: inner_measures,
        filters: inner_filters,
        having: None,
        ..
    }) = inner.non_asap()
    else {
        return None;
    };
    let [inner_sum @ AggIntent::Sum { .. }] = inner_measures.as_slice() else {
        return None;
    };
    if by.is_without() || any_measure_filtered(filters) || any_measure_filtered(inner_filters) {
        return None;
    }
    // The inner sum's value column: a per-series sum keeps its labels and
    // carries the value as the sample value; a grouped one ends with it.
    let value = match inner_reduction {
        Reduction::PerEntity => resolve_column_ref(&ColumnRef::SampleValue, &inner.schema).ok()?,
        Reduction::Reduce(keys) if !keys.is_without() => keys.len(),
        Reduction::Reduce(_) => return None,
    };
    let summed = match col {
        None => resolve_column_ref(&ColumnRef::SampleValue, &inner.schema).ok()?,
        Some(column) => *column,
    };
    if summed != value || by.keys().contains(&value) {
        return None;
    }
    // A per-series sum drops the metric name, which the raw rows keep:
    // grouping them by it would name groups the nested sums leave unnamed.
    if matches!(inner_reduction, Reduction::PerEntity)
        && by.keys().iter().any(|&key| {
            inner
                .schema
                .fields
                .get(key)
                .is_some_and(|f| f.name == "__name__")
        })
    {
        return None;
    }
    let partitions = partitions_over(reduction, &inner.schema, &rows.schema)?;
    Some((Rc::clone(rows), inner_sum.clone(), partitions))
}

/// Whole-expression top-k (Example 1's Q2): a top-k over a per-item `sum`
/// or `count` that is itself a target gets a fused computation
/// ([`ComputationTarget::fused`]) absorbing the inner target, whose
/// candidates are the top-k's heap summaries (Count-Min and CountSketch
/// with a heap), each one heap over the inner aggregate's input, keyed by
/// the ranked item and weighted by the summed value. Those heaps over the
/// exact per-item result leave the top-k's own candidates: the fused
/// computation replaces them. Over a grouped sum of per-series sums or
/// counts of raw samples, the heap reads those samples and absorbs the
/// per-series target too. Applied only when the inner aggregate has no other
/// consumer, so absorbing it removes its work, when neither has a filter or
/// `HAVING`, and when the keyed-additive rule realizes it
/// ([`whole_expression_input`]).
fn add_whole_expression_targets(
    targets: &mut [ComputationTarget],
    consumers: &HashMap<*const OperatorNode, Vec<Option<Rc<OperatorNode>>>>,
) {
    for t in 0..targets.len() {
        let target = &targets[t];
        let Some(NonASAPOp::Aggregate { child, .. }) = target.aggregate.non_asap() else {
            continue;
        };
        let Some(inner) = targets
            .iter()
            .position(|other| Rc::ptr_eq(&other.aggregate, child))
        else {
            continue;
        };
        if consumers.get(&Rc::as_ptr(child)).map(Vec::len) != Some(1)
            || target.filter.is_some()
            || !target.having.is_empty()
            || targets[inner].filter.is_some()
            || !targets[inner].having.is_empty()
        {
            continue;
        }
        let Some((input, ..)) = whole_expression_input(&target.aggregate) else {
            continue;
        };
        // A heap over the raw samples beneath a grouped sum of per-series
        // sums or counts also absorbs the per-series target, when the
        // grouped sum is its only consumer.
        let mut absorbs = vec![inner];
        if let Some(NonASAPOp::Aggregate {
            child: per_series, ..
        }) = targets[inner].aggregate.non_asap()
        {
            let reads_below = per_series
                .children()
                .iter()
                .any(|&rows| Rc::ptr_eq(rows, &input));
            if reads_below && consumers.get(&Rc::as_ptr(per_series)).map(Vec::len) == Some(1) {
                absorbs.extend(
                    targets
                        .iter()
                        .position(|other| Rc::ptr_eq(&other.aggregate, per_series)),
                );
            }
        }
        let (heaps, own): (Vec<_>, Vec<_>) = targets[t]
            .choices
            .candidates
            .iter()
            .cloned()
            .partition(|candidate| {
                matches!(
                    candidate,
                    ComputationCandidate::Summary(
                        SketchAlgorithm::CmsWithHeap | SketchAlgorithm::CountSketchWithHeap
                    )
                )
            });
        if heaps.is_empty() {
            continue;
        }
        let target = &mut targets[t];
        target.choices.candidates = own;
        target.fused = Some(FusedComputation {
            absorbs,
            candidates: heaps,
            pruned: Vec::new(),
        });
    }
}

/// Prunes the own heap candidates of each PromQL top-k
/// ([`PruneReason::TopKHeapOverOneRowPerItem`]); a fused whole-expression
/// heap ([`add_whole_expression_targets`]) has already moved out of them.
fn prune_topk_heaps_over_one_row_per_item(targets: &mut [ComputationTarget]) {
    for target in targets.iter_mut() {
        let Some(NonASAPOp::Aggregate {
            measures, child, ..
        }) = target.aggregate.non_asap()
        else {
            continue;
        };
        if !matches!(measures.get(target.measure), Some(AggIntent::TopK { .. })) {
            continue;
        }
        // A PromQL top-k ranks an instant vector: one row per series or
        // group. (A SQL top-k keeps its heaps.)
        if !reads_only_time_series(child) {
            continue;
        }
        target
            .choices
            .prune(PruneReason::TopKHeapOverOneRowPerItem, |candidate| {
                matches!(
                    candidate,
                    ComputationCandidate::Summary(
                        SketchAlgorithm::CmsWithHeap | SketchAlgorithm::CountSketchWithHeap
                    )
                )
            });
    }
}

/// Whether every scan below `node` reads a time series (PromQL rows).
fn reads_only_time_series(node: &OperatorNode) -> bool {
    match node.non_asap() {
        Some(NonASAPOp::Scan { source, .. }) => matches!(source, Source::TimeSeries { .. }),
        _ => node.children().iter().all(|c| reads_only_time_series(c)),
    }
}

/// The input, update and partitions of a whole-expression top-k over
/// `target`'s inner aggregate, by the keyed-additive rule, or `None` when it
/// does not apply. Rows that carry the full series identity rank it as a
/// column, as [`summary_update`] does. The partitions are `target`'s, moved
/// onto the input the heap reads ([`partitions_over`]).
fn whole_expression_input(
    target: &OperatorNode,
) -> Option<(Rc<OperatorNode>, SummaryUpdate, Reduction)> {
    use crate::realization::realize_keyed_additive_summary_input;
    let Some(NonASAPOp::Aggregate {
        child,
        reduction,
        measures,
        filters,
        having: None,
        ..
    }) = target.non_asap()
    else {
        return None;
    };
    let [intent @ AggIntent::TopK { .. }] = measures.as_slice() else {
        return None;
    };
    if asap_types::ir::operator::non_asap::any_measure_filtered(filters) {
        return None;
    }
    // The rule reads only the family's algorithm, and rejects Count-Min over
    // weights it cannot prove non-negative. Ask with CountSketch so one
    // update serves both heap families; plan selection decides whether
    // Count-Min is admissible, as for every other Count-Min candidate.
    let family = FieldDataType::Sketch(
        SketchKind::new(
            SketchAlgorithm::CountSketchWithHeap,
            asap_types::ir::schema::SketchParams::CountSketchWithHeap {
                width: 1,
                depth: 1,
                heap_size: 1,
            },
        ),
        GroupingStrategy::default(),
    );
    let realized = realize_keyed_additive_summary_input(intent, &family, reduction, child)?;
    let mut input = realized.input;
    let per_series = matches!(
        child.non_asap(),
        Some(NonASAPOp::Aggregate {
            reduction: Reduction::PerEntity,
            ..
        })
    );
    if per_series && realized.child.schema.has_promql_series_identity() {
        input.item = Some(SummaryInputExpr::Column(ColumnRef::Named(
            asap_types::ir::schema::PROMQL_SERIES_IDENTITY.into(),
        )));
    } else if realized.child.schema.closed
        && (matches!(input.item, Some(SummaryInputExpr::EntityIdentity(_)))
            || !reads_only_time_series(&realized.child))
    {
        // An encoded label set needs an open PromQL schema. PromQL rows that
        // carry the series identity are closed, but their inner group labels
        // are item columns. (A SQL top-k keeps its heaps over its per-key
        // result.)
        return None;
    }
    let partitions = partitions_over(reduction, &child.schema, &realized.child.schema)?;
    Some((realized.child, input, partitions))
}

/// `reduction`'s keys, which index `inner` (the absorbed aggregate's
/// output), as indices into `input` (the rows the heap reads), matched by
/// column. `None` when a key is missing or ambiguous in `input`, or the keys
/// are given by `without`: `input` carries columns `inner` dropped.
pub(crate) fn partitions_over(
    reduction: &Reduction,
    inner: &Schema,
    input: &Schema,
) -> Option<Reduction> {
    let Reduction::Reduce(keys) = reduction else {
        return Some(Reduction::PerEntity);
    };
    if keys.is_without() {
        return None;
    }
    let column =
        |schema: &Schema, index: usize| crate::realization::column_ref(&schema.fields[index]);
    let keys = keys
        .keys()
        .iter()
        .map(|&index| {
            inner.fields.get(index)?;
            let key = column(inner, index);
            let mut matches = (0..input.fields.len()).filter(|&i| column(input, i) == key);
            match (matches.next(), matches.next()) {
                (Some(i), None) => Some(i),
                _ => None,
            }
        })
        .collect::<Option<_>>()?;
    Some(Reduction::by(keys))
}

/// The metric whose raw data samples, or plain sums of them, are every value
/// `node` outputs, or `None`. Only a time-series scan, a time range over it,
/// an `offset`-only time shift (it moves the window, not the samples) and a
/// plain `sum` keep that; any other operator (rate and arithmetic included)
/// ends it. The metric's type is not read here: whether its
/// samples are non-negative is plan selection's question
/// ([`WeightDomain::RawDataSamplesOfMetric`]).
pub(crate) fn raw_data_samples_of_metric(node: &OperatorNode) -> Option<String> {
    match node.non_asap()? {
        NonASAPOp::Scan {
            source: Source::TimeSeries { metric },
            ..
        } => Some(metric.clone()),
        NonASAPOp::TimeRange { child, .. } => raw_data_samples_of_metric(child),
        NonASAPOp::TimeShift { shift, child } if shift.at.is_none() => {
            raw_data_samples_of_metric(child)
        }
        NonASAPOp::Aggregate {
            measures, child, ..
        } if matches!(measures.as_slice(), [AggIntent::Sum { col: None }]) => {
            raw_data_samples_of_metric(child)
        }
        _ => None,
    }
}

/// `update` with the open requirement that its weights are samples of
/// `metric` ([`WeightDomain::RawDataSamplesOfMetric`]), when it is a keyed heap
/// update whose weights are not otherwise proven.
fn with_raw_data_samples_requirement(
    mut update: SummaryUpdate,
    metric: Option<&str>,
) -> SummaryUpdate {
    if let Some(metric) = metric {
        if update.item.is_some() && update.weight_domain == WeightDomain::UnknownOrSigned {
            update.weight_domain = WeightDomain::RawDataSamplesOfMetric {
                metric: metric.to_string(),
            };
        }
    }
    update
}

/// Build one whole-workload candidate (end of Pass 1): `choice[i]` picks
/// `inventory.targets[i]`'s candidate, or past its own candidates its fused
/// computation's ([`ComputationTarget::computation_for`]). An aggregate with
/// a chosen candidate other than `Exact` is replaced measure by measure: an
/// exact accumulator by `SummaryAgg` → `FinalizeExactAccumulator`, a summary
/// by `SummaryAgg` → `SummaryEstimate`, `avg` by its `sum` and `count`
/// accumulators divided, a fused computation by one state over the absorbed
/// target's input, and any other measure by its own single-measure
/// aggregate; several measures are joined back on the group keys. A summary
/// is sized here for its measure's accuracy requirement; Pass 1 leaves it
/// unsized; several approximate inputs of one query share its budget
/// equally ([`budget_shares`]). A measure filter becomes `SummaryAgg.filter`;
/// `HAVING` is applied as its target records. A sub-DAG shared between roots
/// stays shared. Every node carries the guarantee [`composed_guarantee`]
/// gives it.
pub fn compose_logical_candidate<Id: Clone>(
    inventory: &PerComputationCandidates<Id>,
    choice: &[usize],
) -> Result<Vec<(Id, QueryRoot)>, PerComputationCandidateError> {
    if choice.len() != inventory.targets.len() {
        return Err(PerComputationCandidateError::InvalidChoice);
    }
    let mut chosen: Chosen = HashMap::new();
    for (target, &index) in inventory.targets.iter().zip(choice) {
        let (candidate, absorbs) = target
            .computation_for(index)
            .ok_or(PerComputationCandidateError::InvalidChoice)?;
        chosen
            .entry(Rc::as_ptr(&target.aggregate))
            .or_default()
            .push(ChosenComputation {
                target,
                candidate,
                fused: absorbs.is_some(),
            });
    }
    // Consumer `Filter`s whose key-only `HAVING` moves below a substituted
    // aggregate: each is replaced by its input.
    let pushed_below: HashSet<_> = chosen
        .values()
        .filter(|targets| substituted(targets))
        .flat_map(|targets| &targets[0].target.having)
        .filter(|entry| entry.placement == HavingPlacement::BelowAggregate)
        .filter_map(|entry| entry.filter.as_ref().map(Rc::as_ptr))
        .collect();
    // Each summary is first sized for its measure's whole budget. A query
    // whose value composes several approximate inputs is then composed
    // again with each sized for an equal share ([`budget_shares`]).
    let whole = Sizing::default();
    let roots = compose_roots(inventory, &chosen, &pushed_below, &whole)?;
    let shares = budget_shares(&roots, &whole.built.into_inner());
    if shares.is_empty() {
        return Ok(roots);
    }
    let shared = Sizing {
        shares,
        ..Sizing::default()
    };
    compose_roots(inventory, &chosen, &pushed_below, &shared)
}

/// One measure of an aggregate: the aggregate and the measure's index.
type MeasureKey = (*const OperatorNode, usize);

/// How [`compose_logical_candidate`] sizes summaries in one composition.
#[derive(Default)]
struct Sizing {
    /// How many approximate inputs each measure's summary shares its
    /// query's budget with; a measure not listed has the whole budget.
    shares: HashMap<MeasureKey, usize>,
    /// Every sketch summary state built, with the measure it sizes.
    built: std::cell::RefCell<Vec<(Rc<OperatorNode>, MeasureKey)>>,
}

/// The equal budget split among the approximate inputs of each query
/// (#669b): a query whose value carries a guarantee that is not exact
/// composes every sketch summary state below its root
/// ([`approximate_inputs`]), so with `n ≥ 2` of them each is sized for one
/// of `n` equal shares of the query's target. A state several queries read
/// takes the largest `n`. Only states `built` lists (by the measure they
/// size) are split. Empty when no query composes two approximate inputs.
fn budget_shares(
    roots: &[(impl Sized, QueryRoot)],
    built: &[(Rc<OperatorNode>, MeasureKey)],
) -> HashMap<MeasureKey, usize> {
    let measure: HashMap<_, _> = built
        .iter()
        .map(|(state, key)| (Rc::as_ptr(state), *key))
        .collect();
    let mut shares = HashMap::new();
    for (_, root) in roots {
        let inputs = approximate_inputs(root);
        if inputs.len() < 2 {
            continue;
        }
        for state in &inputs {
            if let Some(key) = measure.get(&Rc::as_ptr(state)) {
                let share = shares.entry(*key).or_insert(1);
                *share = inputs.len().max(*share);
            }
        }
    }
    shares
}

/// The approximate inputs a query's value composes: every sketch summary
/// state reachable from `root`, each once, when the value carries a
/// guarantee that is not exact ([`root_guarantee`]). Empty when it carries
/// none (no rule composes the inputs) or an exact one. Plan selection sizes
/// each for its share of the query's failure probability too.
pub fn approximate_inputs(root: &QueryRoot) -> Vec<Rc<OperatorNode>> {
    if root_guarantee(root).is_none_or(|g| g.is_exact()) {
        return Vec::new();
    }
    let operators = match root {
        QueryRoot::Operator(node) => vec![node],
        QueryRoot::Scalar(expr) => expr.operator_refs(),
    };
    let mut seen = HashSet::new();
    operators
        .into_iter()
        .flat_map(OperatorNode::reachable)
        .filter(|node| {
            matches!(
                node.asap(),
                Some(
                    ASAPOp::SummaryAgg {
                        family: FieldDataType::Sketch(..),
                        ..
                    } | ASAPOp::PaneChain {
                        family: FieldDataType::Sketch(..),
                        ..
                    }
                )
            )
        })
        .filter(|node| seen.insert(Rc::as_ptr(node)))
        .collect()
}

/// The theoretical guarantee of a query's output, as composed: the root
/// node's own, or a scalar root's composed over the operators it reads
/// ([`scalar_guarantee`]).
pub fn root_guarantee(root: &QueryRoot) -> Option<AccuracyGuarantee> {
    match root {
        QueryRoot::Operator(node) => node.guarantee.clone(),
        QueryRoot::Scalar(expr) => scalar_guarantee(expr, &|_| None),
    }
}

/// `roots` with every sketch summary state rebuilt with the parameters
/// `resize` gives it (same algorithm; `None` keeps them), and every node
/// above one rebuilt over its new inputs with its guarantee composed again
/// ([`composed_guarantee`]; a node that had none keeps none). Timing,
/// materialization and sharing are kept. Plan selection enlarges summaries
/// for a failure probability this way.
pub fn with_resized_summaries(
    roots: &[QueryRoot],
    resize: &dyn Fn(&OperatorNode, &SketchParams) -> Option<SketchParams>,
) -> Result<Vec<QueryRoot>, SchemaDerivationError> {
    fn walk(
        node: &Rc<OperatorNode>,
        resize: &dyn Fn(&OperatorNode, &SketchParams) -> Option<SketchParams>,
        memo: &mut Memo,
    ) -> Result<Rc<OperatorNode>, SchemaDerivationError> {
        if let Some(done) = memo.get(&Rc::as_ptr(node)) {
            return Ok(done.clone());
        }
        for child in node.children() {
            walk(child, resize, memo)?;
        }
        let mut changed = node
            .children()
            .iter()
            .any(|child| !Rc::ptr_eq(child, &memo[&Rc::as_ptr(child)]));
        let mut operator = node.operator.clone();
        if let Operator::ASAP(
            ASAPOp::SummaryAgg {
                family: FieldDataType::Sketch(kind, _),
                ..
            }
            | ASAPOp::PaneChain {
                family: FieldDataType::Sketch(kind, _),
                ..
            },
        ) = &mut operator
        {
            if let Some(params) = resize(node, kind.params()).filter(|p| p != kind.params()) {
                *kind = SketchKind::new(kind.algorithm().clone(), params);
                changed = true;
            }
        }
        let rebuilt = if changed {
            let template = OperatorNode {
                operator,
                ..(**node).clone()
            };
            let mut rebuilt =
                template.with_new_children(|child| memo[&Rc::as_ptr(child)].clone())?;
            rebuilt.guarantee = node
                .guarantee
                .as_ref()
                .and_then(|_| composed_guarantee(&rebuilt));
            Rc::new(
                rebuilt
                    .with_timing(node.timing)
                    .with_materialization(node.materialization),
            )
        } else {
            node.clone()
        };
        memo.insert(Rc::as_ptr(node), rebuilt.clone());
        Ok(rebuilt)
    }
    let mut memo = HashMap::new();
    roots
        .iter()
        .map(|root| {
            Ok(match root {
                QueryRoot::Operator(node) => QueryRoot::Operator(walk(node, resize, &mut memo)?),
                QueryRoot::Scalar(expr) => {
                    let mut rebuilt = HashMap::new();
                    for node in expr.operator_refs() {
                        rebuilt.insert(Rc::as_ptr(node), walk(node, resize, &mut memo)?);
                    }
                    QueryRoot::Scalar(
                        expr.map_operator_refs(&mut |node| rebuilt[&Rc::as_ptr(node)].clone()),
                    )
                }
            })
        })
        .collect()
}

/// The one source a summary state counts the distinct items of its own
/// rows of, as [`source_distinct_guarantee`] reads it: the source whose
/// distinct bound gives its failure probability. `None` for a state over
/// computed rows or several sources.
pub fn distinct_bound_source(state: &OperatorNode) -> Option<String> {
    let Some(ASAPOp::SummaryAgg { child, .. } | ASAPOp::PaneChain { child, .. }) = state.asap()
    else {
        return None;
    };
    source_scan(child)?;
    source(child)
}

/// Every root of `inventory` with the chosen candidates substituted, each
/// summary sized as `sizing` says, and every node with its guarantee.
fn compose_roots<Id: Clone>(
    inventory: &PerComputationCandidates<Id>,
    chosen: &Chosen<'_>,
    pushed_below: &HashSet<*const OperatorNode>,
    sizing: &Sizing,
) -> Result<Vec<(Id, QueryRoot)>, PerComputationCandidateError> {
    let (mut memo, mut guaranteed) = (HashMap::new(), HashMap::new());
    let mut rewrite = |node: &Rc<OperatorNode>| {
        let node = rewrite(node, chosen, pushed_below, sizing, &mut memo)?;
        with_guarantees(&node, &mut guaranteed)
    };
    let roots = inventory
        .roots
        .iter()
        .map(|(id, root)| {
            let root = match root {
                QueryRoot::Operator(node) => QueryRoot::Operator(rewrite(node)?),
                QueryRoot::Scalar(expr) => {
                    let mut rewritten = HashMap::new();
                    for node in expr.operator_refs() {
                        rewritten.insert(Rc::as_ptr(node), rewrite(node)?);
                    }
                    QueryRoot::Scalar(
                        expr.map_operator_refs(&mut |node| rewritten[&Rc::as_ptr(node)].clone()),
                    )
                }
            };
            // A substitution this module cannot express is an error, never an
            // invalid DAG.
            root.validate_structure()?;
            Ok((id.clone(), root))
        })
        .collect::<Result<Vec<_>, PerComputationCandidateError>>()?;
    // The built states, as the roots hold them: with their guarantees.
    for (state, _) in sizing.built.borrow_mut().iter_mut() {
        if let Some(with_guarantee) = guaranteed.get(&Rc::as_ptr(state)) {
            *state = with_guarantee.clone();
        }
    }
    Ok(roots)
}

/// One target's chosen computation in a whole-workload choice.
struct ChosenComputation<'a> {
    /// The target whose measure it computes.
    target: &'a ComputationTarget,
    /// How it is computed.
    candidate: &'a ComputationCandidate,
    /// Whether the candidate is the target's fused computation's, which
    /// absorbs the targets beneath; `false` for one of its own candidates.
    fused: bool,
}

/// Each aggregate's targets with their chosen computations.
type Chosen<'a> = HashMap<*const OperatorNode, Vec<ChosenComputation<'a>>>;
type Memo = HashMap<*const OperatorNode, Rc<OperatorNode>>;

/// Whether any measure of the aggregate is substituted by something other than
/// its plain exact computation.
fn substituted(targets: &[ChosenComputation<'_>]) -> bool {
    targets
        .iter()
        .any(|chosen| *chosen.candidate != ComputationCandidate::Exact)
}

fn rewrite(
    node: &Rc<OperatorNode>,
    chosen: &Chosen<'_>,
    pushed_below: &HashSet<*const OperatorNode>,
    sizing: &Sizing,
    memo: &mut Memo,
) -> Result<Rc<OperatorNode>, PerComputationCandidateError> {
    if let Some(done) = memo.get(&Rc::as_ptr(node)) {
        return Ok(done.clone());
    }
    for child in node.children() {
        rewrite(child, chosen, pushed_below, sizing, memo)?;
    }
    let changed = node
        .children()
        .iter()
        .any(|child| !Rc::ptr_eq(child, &memo[&Rc::as_ptr(child)]));
    let rebuilt = match (chosen.get(&Rc::as_ptr(node)), node.non_asap()) {
        (Some(targets), _) if substituted(targets) => substitute(node, targets, sizing, memo)?,
        (_, Some(NonASAPOp::Filter { child, .. })) if pushed_below.contains(&Rc::as_ptr(node)) => {
            memo[&Rc::as_ptr(child)].clone()
        }
        _ if changed => Rc::new(node.with_new_children(|child| memo[&Rc::as_ptr(child)].clone())?),
        _ => node.clone(),
    };
    memo.insert(Rc::as_ptr(node), rebuilt.clone());
    Ok(rebuilt)
}

/// `roots` with every node's guarantee composed again
/// ([`composed_guarantee`]), children first; a sub-DAG shared between roots
/// stays shared. Pass 2 calls it after re-sizing a shared summary, so each
/// consumer's guarantee is the shared state's.
pub(crate) fn recompose_guarantees(
    roots: &[QueryRoot],
) -> Result<Vec<QueryRoot>, PerComputationCandidateError> {
    let mut memo = HashMap::new();
    roots
        .iter()
        .map(|root| {
            Ok(match root {
                QueryRoot::Operator(node) => QueryRoot::Operator(with_guarantees(node, &mut memo)?),
                QueryRoot::Scalar(expr) => {
                    for node in expr.operator_refs() {
                        with_guarantees(node, &mut memo)?;
                    }
                    QueryRoot::Scalar(
                        expr.map_operator_refs(&mut |node| memo[&Rc::as_ptr(node)].clone()),
                    )
                }
            })
        })
        .collect()
}

/// `node` and everything below it, each node with its [`composed_guarantee`].
fn with_guarantees(
    node: &Rc<OperatorNode>,
    memo: &mut Memo,
) -> Result<Rc<OperatorNode>, PerComputationCandidateError> {
    if let Some(done) = memo.get(&Rc::as_ptr(node)) {
        return Ok(done.clone());
    }
    for child in node.children() {
        with_guarantees(child, memo)?;
    }
    let mut rebuilt = node.with_new_children(|child| memo[&Rc::as_ptr(child)].clone())?;
    rebuilt.guarantee = composed_guarantee(&rebuilt);
    let rebuilt = Rc::new(rebuilt);
    memo.insert(Rc::as_ptr(node), rebuilt.clone());
    Ok(rebuilt)
}

/// The theoretical guarantee of a composed node, given its children's,
/// composed with the built-in analytical rules ([`propagate`]);
/// Stage 1 reads no data statistics or deployment evidence. Exact over exact
/// inputs; a summary estimate has its sized family's bound when
/// [`estimate_has_bound`], composed with its input's when that is
/// approximate; an exact aggregation or accumulator over approximate values
/// uses its function's rule ([`approximate_input_composition`]); arithmetic composes as
/// [`arithmetic_guarantee`]. A rule that needs a statistic leaves it
/// symbolic (`BoundExpr::Unknown`), and plan selection (Stage 3) binds it.
/// `None` where no rule applies: a comparison, a value `HAVING` or any other
/// predicate over an approximate value, a rank error through any operator (a
/// rank interval counts as none in the MVP). A projection of columns only
/// keeps its child's guarantee. A summary state carries none itself.
///
/// A guarantee holds per output point (per series or group), with its
/// failure probability per point ([`AccuracyScope::PerValue`]). Plan
/// selection states it over a whole result of `G` points as `G·δ`.
///
/// [`AccuracyScope::PerValue`]: asap_types::ir::properties::AccuracyScope::PerValue
fn composed_guarantee(node: &OperatorNode) -> Option<AccuracyGuarantee> {
    if let Some(guarantee) = merged_panes_guarantee(node) {
        return guarantee;
    }
    let exact = |node: &OperatorNode| {
        node.guarantee
            .as_ref()
            .is_some_and(AccuracyGuarantee::is_exact)
    };
    // A summary state's input rows and any subquery in its filter.
    let exact_inputs = |state: &OperatorNode| state.children().into_iter().all(|c| exact(c));
    // Over approximate input rows, a summary's filter must not decide which
    // rows count by an estimate: it may read no input column (guarantees are
    // per node, so any column may carry the estimate; column-level tracking
    // is #689), and its subqueries must be exact.
    let exact_filter = |state: &OperatorNode| {
        let Some(ASAPOp::SummaryAgg { filter, .. }) = state.asap() else {
            return false;
        };
        filter
            .as_ref()
            .is_none_or(|filter| !reads_columns(&filter.0))
            && state.children().into_iter().skip(1).all(|c| exact(c))
    };
    match &node.operator {
        Operator::ASAP(ASAPOp::FinalizeExactAccumulator { child }) => match child.asap() {
            Some(ASAPOp::SummaryAgg {
                family: FieldDataType::ExactAggregate(..),
                ..
            }) if exact_inputs(child) => Some(AccuracyGuarantee::exact(
                "exact accumulator over exact input",
            )),
            Some(ASAPOp::SummaryAgg {
                family: FieldDataType::ExactAggregate(kind, _),
                child: input,
                reduction,
                ..
            }) if exact_filter(child) && !groups_by_estimate(reduction, input) => {
                let stats = fold_stats(reduction, input)?;
                let input = operand_guarantee(input)?;
                let op = match kind {
                    // Counts rows, not values.
                    ExactKind::Count => return Some(AccuracyGuarantee::exact("count of rows")),
                    ExactKind::Sum => CompositionOperator::ExactSum,
                    ExactKind::Min | ExactKind::Max => CompositionOperator::ExactExtremum,
                    ExactKind::Rate => CompositionOperator::CounterRate,
                    ExactKind::IRate => CompositionOperator::InstantCounterRate,
                    ExactKind::Increase => CompositionOperator::CounterIncrease,
                };
                propagate(&op, &[input], None, &stats).ok()
            }
            _ => None,
        },
        Operator::ASAP(ASAPOp::SummaryEstimate {
            summary_input,
            query,
        }) => match summary_input.asap() {
            Some(ASAPOp::SummaryAgg {
                family,
                input: update,
                child: input,
                reduction,
                ..
            }) if estimate_has_bound(family, update, query)
                || estimate_bound_over_raw_data_samples(family, update, query).is_some() =>
            {
                let local = match estimate_bound_over_raw_data_samples(family, update, query) {
                    Some(gated) => gated,
                    None => source_distinct_guarantee(family, query, input)
                        .or_else(|| local_guarantee(family, query))?,
                };
                if exact_inputs(summary_input) {
                    Some(local)
                } else if exact_filter(summary_input) && !groups_by_estimate(reduction, input) {
                    compose(
                        &CompositionOperator::ApproximateAggregate,
                        vec![input.guarantee.clone()?],
                        Some(&local),
                    )
                } else {
                    None
                }
            }
            _ => None,
        },
        Operator::ASAP(_) => None,
        // Renaming or reordering columns keeps the child's guarantee.
        Operator::NonASAP(NonASAPOp::Project { cols, child, .. })
            if cols
                .iter()
                .all(|item| matches!(item.expr, ScalarExpr::Column(_))) =>
        {
            child.guarantee.clone()
        }
        // Each step re-evaluates the child: every sample keeps its guarantee.
        Operator::NonASAP(NonASAPOp::PromqlSubquery { child, .. }) => child.guarantee.clone(),
        Operator::NonASAP(_) if node.children().into_iter().all(|child| exact(child)) => {
            Some(AccuracyGuarantee::exact("exact over exact inputs"))
        }
        // Over a join, the child's sample value may be an exact input's
        // column, so each column's own guarantee is read instead.
        Operator::NonASAP(NonASAPOp::Project { cols, child, .. }) if derives_from_join(child) => {
            relational_project_guarantee(cols, child)
        }
        Operator::NonASAP(NonASAPOp::Project { cols, child, .. }) => project_guarantee(cols, child),
        Operator::NonASAP(NonASAPOp::Join { .. }) => join_guarantee(node),
        // A measure filter or `HAVING` over approximate values changes
        // which rows or groups count: no rule.
        Operator::NonASAP(NonASAPOp::Aggregate {
            measures,
            filters,
            having: None,
            reduction,
            child,
            ..
        }) if filters.iter().all(Option::is_none) && !groups_by_estimate(reduction, child) => {
            match measures.as_slice() {
                [intent] => propagate(
                    &approximate_input_composition(intent)?,
                    &[operand_guarantee(child)?],
                    None,
                    &fold_stats(reduction, child)?,
                )
                .ok(),
                _ => None,
            }
        }
        Operator::NonASAP(NonASAPOp::BinaryOp {
            operator:
                BinaryOperator {
                    kind: BinaryOpKind::Arithmetic(op),
                    ..
                },
            lhs,
            rhs,
            ..
        }) => arithmetic_guarantee(
            op.clone(),
            (operand_guarantee(lhs), None),
            (operand_guarantee(rhs), None),
            Rc::ptr_eq(lhs, rhs),
        ),
        Operator::NonASAP(_) => None,
    }
}

/// The guarantee of an estimate or finalized value read from merged
/// tumbling panes ([`merged_chain`]): the guarantee of the same read of one
/// build over the panes' whole window ([`whole_window_build`]) when the
/// family's merged error for that read is exact or the same as that build's
/// (the family-capability table); none when it is approximate (evidence
/// only, for plan selection to judge) or the family has no merge. `None` for
/// any node that does not read merged panes.
fn merged_panes_guarantee(node: &OperatorNode) -> Option<Option<AccuracyGuarantee>> {
    let (state, merged) = match node.asap()? {
        ASAPOp::SummaryEstimate {
            summary_input,
            query,
        } => {
            let family = merged_chain(summary_input)?.asap()?.produced_state()?;
            (
                summary_input,
                FamilyCapability::of(family).merged_error(query),
            )
        }
        ASAPOp::FinalizeExactAccumulator { child } => {
            let family = merged_chain(child)?.asap()?.produced_state()?;
            // An exact accumulator's merged state is exact.
            let merged = FamilyCapability::of(family).merge_inputs;
            (child, merged.map(|_| MergedError::Exact))
        }
        _ => return None,
    };
    if !matches!(
        merged,
        Some(MergedError::Exact | MergedError::AsIfBuiltOverUnion)
    ) {
        return Some(None);
    }
    let chain = merged_chain(state)?;
    let Ok(mut whole) = whole_window_build(chain) else {
        return Some(None);
    };
    // The whole window's range over the chain's input carries the guarantee
    // composed over that input.
    let ASAPOp::PaneChain { child: input, .. } = chain.asap()? else {
        return Some(None);
    };
    if let Operator::ASAP(ASAPOp::SummaryAgg { child: range, .. }) = &mut whole.operator {
        let mut timed = (**range).clone();
        timed.guarantee = composed_guarantee(&timed);
        debug_assert!(Rc::ptr_eq(timed.children()[0], input));
        *range = Rc::new(timed);
    }
    let whole = Rc::new(whole);
    let reader = OperatorNode::with_schema(
        node.operator.map_children(|child| {
            if Rc::ptr_eq(child, state) {
                Rc::clone(&whole)
            } else {
                Rc::clone(child)
            }
        }),
        node.schema.clone(),
    );
    Some(composed_guarantee(&reader))
}

/// The guarantee of a distinct count of one instance per group over a
/// source's own rows, whose failure probability is left symbolic in the
/// source's distinct bound, which Stage 1 does not read:
/// - classic HLL's ([`hll::classic_guarantee`]);
/// - UnivMon's, sized for a distinct count ([`univmon::distinct_guarantee`]).
///
/// `None` for any other summary, or one over computed rows, whose distinct
/// items the source's bound does not bound.
fn source_distinct_guarantee(
    family: &FieldDataType,
    query: &SketchStatistic,
    input: &Rc<OperatorNode>,
) -> Option<AccuracyGuarantee> {
    let FieldDataType::Sketch(kind, GroupingStrategy::PerSubpopulationInstance) = family else {
        return None;
    };
    if *query != SketchStatistic::Cardinality {
        return None;
    }
    source_scan(input)?;
    match kind.params() {
        SketchParams::Hll {
            precision,
            relative_error,
        } => hll::classic_guarantee(*precision, *relative_error, &source(input)?),
        params @ SketchParams::UnivMon { .. } => {
            univmon::distinct_guarantee(params, &source(input)?)
        }
        _ => None,
    }
}

/// The rows one group of an exact fold over approximate `input` counts.
/// Across PromQL series (`Reduce`) they are at most the input series, an
/// input-row-count statistic left symbolic for plan selection; across SQL
/// groups they are a group-row-count statistic nothing binds; over time
/// (`PerEntity`) they are one series' samples in the window: the steps of a
/// subquery with an explicit resolution, `⌈range / resolution⌉`. `None`
/// when that count is not in the query.
fn fold_stats(reduction: &Reduction, input: &Rc<OperatorNode>) -> Option<PropagationStats> {
    let sign_statistic =
        estimates_raw_data_samples_of(input).map(|metric| raw_data_samples_sign_statistic(&metric));
    let stats = match reduction {
        // The series count of the sources read bounds a PromQL fold's rows
        // (series), not the rows of a SQL group.
        Reduction::Reduce(_) if reads_table(input) => Some(PropagationStats {
            row_count_statistic: Some(GROUP_ROW_COUNT),
            ..PropagationStats::default()
        }),
        Reduction::Reduce(_) => Some(PropagationStats::default()),
        Reduction::PerEntity => match input.non_asap() {
            Some(NonASAPOp::PromqlSubquery {
                range,
                resolution: Some(resolution),
                ..
            }) if !resolution.is_zero() => Some(PropagationStats {
                input_row_count: Some(range.as_nanos().div_ceil(resolution.as_nanos()) as u64),
                ..PropagationStats::default()
            }),
            _ => None,
        },
    }?;
    Some(PropagationStats {
        sign_statistic,
        ..stats
    })
}

/// The metric whose raw data samples, or plain sums of them, a composed node's
/// values are or estimate, or `None`: [`raw_data_samples_of_metric`]'s strict rule
/// over a composed candidate. An exact sum accumulator, a projection of
/// columns and a
/// heap estimate whose update weights carry the
/// [`WeightDomain::RawDataSamplesOfMetric`] requirement keep it, and so does a
/// quantile estimate of those values, built over the raw window or merged
/// from its tumbling panes: it is one of them, or between two of them, so it
/// shares their sign. Anything else (rate, arithmetic, other
/// functions) ends it. Only the sign of the values is read from it.
fn estimates_raw_data_samples_of(node: &OperatorNode) -> Option<String> {
    match &node.operator {
        Operator::ASAP(ASAPOp::FinalizeExactAccumulator { child }) => match child.asap()? {
            ASAPOp::SummaryAgg {
                family: FieldDataType::ExactAggregate(ExactKind::Sum, _),
                child,
                filter: None,
                ..
            } => estimates_raw_data_samples_of(child),
            _ => None,
        },
        // Only a φ in [0, 1] picks one of the values; outside it the answer
        // is -Inf or +Inf, whatever their sign. Merged tumbling panes of one
        // definition ([`merged_chain`]) are read through their `PaneChain`:
        // a quantile of the merged state is still one of the panes'
        // samples, or between two of them.
        Operator::ASAP(ASAPOp::SummaryEstimate {
            summary_input,
            query: SketchStatistic::Quantile { q },
        }) if (0.0..=1.0).contains(q) => match merged_chain(summary_input)
            .unwrap_or(summary_input)
            .asap()?
        {
            ASAPOp::SummaryAgg {
                input:
                    SummaryUpdate {
                        item: None,
                        weight: SummaryInputExpr::Column(ColumnRef::SampleValue),
                        ..
                    },
                filter: None,
                child,
                ..
            }
            | ASAPOp::PaneChain {
                input:
                    SummaryUpdate {
                        item: None,
                        weight: SummaryInputExpr::Column(ColumnRef::SampleValue),
                        ..
                    },
                filter: None,
                child,
                ..
            } => estimates_raw_data_samples_of(child),
            _ => None,
        },
        Operator::ASAP(ASAPOp::SummaryEstimate { summary_input, .. }) => {
            match summary_input.asap()? {
                ASAPOp::SummaryAgg {
                    input:
                        SummaryUpdate {
                            weight_domain: WeightDomain::RawDataSamplesOfMetric { metric },
                            ..
                        },
                    filter: None,
                    ..
                } => Some(metric.clone()),
                _ => None,
            }
        }
        // Renaming, reordering or dropping columns keeps every value.
        Operator::NonASAP(NonASAPOp::Project { cols, child, .. })
            if cols
                .iter()
                .all(|item| matches!(item.expr, ScalarExpr::Column(_))) =>
        {
            estimates_raw_data_samples_of(child)
        }
        _ => raw_data_samples_of_metric(node),
    }
}

/// Whether `node` reads a SQL table.
fn reads_table(node: &Rc<OperatorNode>) -> bool {
    OperatorNode::reachable(node).iter().any(|node| {
        matches!(
            node.non_asap(),
            Some(NonASAPOp::Scan {
                source: Source::Table { .. },
                ..
            })
        )
    })
}

/// Whether a grouping by `reduction` over an approximate `input` may decide
/// a row's group by an estimate: a grouping column other than a label (a
/// string) or the time index. Guarantees are per node, so any such column
/// may carry the estimate (column-level tracking is #689); a PromQL `by`
/// or `without` groups by labels only. `false` over exact input.
fn groups_by_estimate(reduction: &Reduction, input: &OperatorNode) -> bool {
    let Some(by) = reduction.group_keys() else {
        return false;
    };
    if input
        .guarantee
        .as_ref()
        .is_some_and(AccuracyGuarantee::is_exact)
    {
        return false;
    }
    let schema = &input.schema;
    let columns: Vec<usize> = if by.is_without() {
        let value = resolve_column_ref(&ColumnRef::SampleValue, schema).ok();
        (0..schema.fields.len())
            .filter(|i| !by.keys().contains(i) && Some(*i) != value)
            .collect()
    } else {
        by.keys().to_vec()
    };
    columns.into_iter().any(|i| {
        Some(i) != schema.time_index
            && !matches!(
                schema.fields.get(i).map(|f| &f.dtype),
                Some(FieldDataType::Plain(DataType::Utf8))
            )
    })
}

/// Whether `expr` reads any input column.
fn reads_columns(expr: &ScalarExpr) -> bool {
    let mut reads = false;
    expr.map_columns(&mut |column| {
        reads = true;
        column
    });
    reads
}

/// The built-in rule for `op`, with every statistic unknown (Stage 1 reads
/// none); `None` when no rule applies.
fn compose(
    op: &CompositionOperator,
    inputs: Vec<AccuracyGuarantee>,
    local: Option<&AccuracyGuarantee>,
) -> Option<AccuracyGuarantee> {
    propagate(op, &inputs, local, &PropagationStats::default()).ok()
}

/// A projection that computes over its child's sample value: the guarantee
/// of the one item that reads it ([`project_column_guarantee`]). `None` when
/// no single item reads the value.
fn project_guarantee(cols: &[ProjectItem], child: &Rc<OperatorNode>) -> Option<AccuracyGuarantee> {
    let value = resolve_column_ref(&ColumnRef::SampleValue, &child.schema).ok()?;
    let reads_value = |expr: &ScalarExpr| {
        let mut reads = false;
        expr.map_columns(&mut |column| {
            reads |= column == value;
            column
        });
        reads
    };
    let [item] = <[_; 1]>::try_from(
        cols.iter()
            .filter(|item| reads_value(&item.expr))
            .collect::<Vec<_>>(),
    )
    .ok()?;
    scalar_guarantee(&item.expr, &|column| {
        project_column_guarantee(child, column)
    })
}

/// Whether `node` is a join, or reads one only through operators that keep
/// its columns (projections, filters, sorts, limits, deduplication). The
/// columns of such a node come from different inputs, so its sample value
/// says nothing about where the error lies.
fn derives_from_join(node: &OperatorNode) -> bool {
    match node.non_asap() {
        Some(NonASAPOp::Join { .. }) => true,
        Some(
            NonASAPOp::Project { child, .. }
            | NonASAPOp::Filter { child, .. }
            | NonASAPOp::Sort { child, .. }
            | NonASAPOp::Limit { child, .. }
            | NonASAPOp::Dedup { child, .. },
        ) => derives_from_join(child),
        _ => false,
    }
}

/// The guarantee of each output column of `node`, where it is known: every
/// column of an exact node is exact; a join beside exact inputs
/// ([`join_guarantee`]) gives the exact inputs' columns exact and the
/// approximate input's columns its guarantee; a projection of columns maps
/// its child's. `None` anywhere else, a computed projection included.
/// Guarantees are per node, so every column of an approximate input counts
/// as approximate; column-level tracking is #689.
///
/// This assumes every approximate node of Stage 1 has an exact row set:
/// its guarantee bounds each row's values, never which rows exist (a
/// top-k estimate, whose rows are approximate, has no bound;
/// `estimate_has_bound`). A guarantee on row membership would need these
/// rules revisited.
fn column_guarantees(node: &OperatorNode) -> Option<Vec<AccuracyGuarantee>> {
    let width = node.schema.fields.len();
    if let Some(guarantee) = node.guarantee.as_ref().filter(|g| g.is_exact()) {
        return Some(vec![guarantee.clone(); width]);
    }
    match &node.operator {
        Operator::NonASAP(NonASAPOp::Join {
            kind:
                JoinKind::Inner | JoinKind::Left | JoinKind::Right | JoinKind::Full | JoinKind::Cross,
            left,
            right,
            ..
        }) => {
            let input = join_input_guarantees(left, right)?;
            (input.len() == width).then_some(input)
        }
        Operator::NonASAP(NonASAPOp::Project { cols, child, .. }) => {
            let child = column_guarantees(child)?;
            cols.iter()
                .map(|item| match item.expr {
                    ScalarExpr::Column(id) => child.get(id).cloned(),
                    _ => None,
                })
                .collect()
        }
        _ => None,
    }
}

/// The guarantees of a join's input columns (`left ++ right`) when at most
/// one input is approximate and the others are exact: the exact input's
/// columns exact, the approximate input's columns its guarantee.
fn join_input_guarantees(
    left: &OperatorNode,
    right: &OperatorNode,
) -> Option<Vec<AccuracyGuarantee>> {
    let exact = |node: &OperatorNode| node.guarantee.as_ref().is_some_and(|g| g.is_exact());
    if !exact(left) && !exact(right) {
        return None;
    }
    let side = |node: &OperatorNode| {
        let guarantee = node.guarantee.clone()?;
        Some(vec![guarantee; node.schema.fields.len()])
    };
    let mut columns = side(left)?;
    columns.extend(side(right)?);
    Some(columns)
}

/// A join whose inputs are exact but one keeps that input's guarantee when
/// its predicate reads only exact columns (a cross join, or a key equality
/// on the exact inputs' columns): which rows match, and how often, then
/// does not depend on an estimate, and each output row carries one row of
/// the approximate input unchanged. Outer joins pad on the same exact
/// decision. A semi- or anti-join, a key on an approximate input's column
/// (per-node guarantees count all its columns approximate; #689), or two
/// approximate inputs have none. An exact join is composed before this.
/// Like [`column_guarantees`], this assumes the approximate input's row set
/// is exact: only its values carry error.
fn join_guarantee(node: &OperatorNode) -> Option<AccuracyGuarantee> {
    let Operator::NonASAP(NonASAPOp::Join {
        kind: JoinKind::Inner | JoinKind::Left | JoinKind::Right | JoinKind::Full | JoinKind::Cross,
        pred,
        left,
        right,
    }) = &node.operator
    else {
        return None;
    };
    let columns = join_input_guarantees(left, right)?;
    let pred_exact =
        scalar_guarantee(&pred.0, &|id| columns.get(id).cloned()).is_some_and(|g| g.is_exact());
    if !pred_exact {
        return None;
    }
    [left, right]
        .into_iter()
        .find_map(|input| input.guarantee.clone().filter(|g| !g.is_exact()))
}

/// The guarantee of a projection over relational columns
/// ([`column_guarantees`]): each output column's as [`scalar_guarantee`]
/// (a `CASE` as [`case_guarantee`]), and the node's the weakest of them.
/// `None` when the child's column guarantees are unknown or any output
/// column has none.
fn relational_project_guarantee(
    cols: &[ProjectItem],
    child: &Rc<OperatorNode>,
) -> Option<AccuracyGuarantee> {
    let columns = column_guarantees(child)?;
    let column = |id: usize| columns.get(id).cloned();
    weakest(
        cols.iter()
            .map(|item| scalar_guarantee(&item.expr, &column))
            .collect::<Option<Vec<_>>>()?,
    )
}

/// The weakest of `guarantees`: exact when all are, the approximate one
/// when there is one (or all approximate ones are equal), and otherwise
/// as an extremum of them ([`lipschitz_guarantee`]).
fn weakest(guarantees: Vec<AccuracyGuarantee>) -> Option<AccuracyGuarantee> {
    let mut approximate: Vec<AccuracyGuarantee> = Vec::new();
    for guarantee in guarantees.into_iter().filter(|g| !g.is_exact()) {
        if !approximate.contains(&guarantee) {
            approximate.push(guarantee);
        }
    }
    match approximate.len() {
        0 => Some(AccuracyGuarantee::exact("exact over exact operands")),
        1 => approximate.pop(),
        _ => lipschitz_guarantee(approximate),
    }
}

/// A `CASE` whose operand and conditions are exact picks its branch by an
/// exact decision, so it keeps the weakest branch's guarantee (a literal or
/// NULL branch, or a missing `ELSE`, is exact). A condition over an
/// approximate value has none.
fn case_guarantee(
    operand: Option<&ScalarExpr>,
    branches: &[(ScalarExpr, ScalarExpr)],
    else_expr: Option<&ScalarExpr>,
    column: &dyn Fn(usize) -> Option<AccuracyGuarantee>,
) -> Option<AccuracyGuarantee> {
    let exact = |expr: &ScalarExpr| scalar_guarantee(expr, column).is_some_and(|g| g.is_exact());
    if !operand.is_none_or(exact) || !branches.iter().all(|(when, _)| exact(when)) {
        return None;
    }
    weakest(
        branches
            .iter()
            .map(|(_, then)| then)
            .chain(else_expr)
            .map(|expr| scalar_guarantee(expr, column))
            .collect::<Option<Vec<_>>>()?,
    )
}

/// The guarantee of `column` of a projection's `child`: over a join, the
/// column's own ([`column_guarantees`]); otherwise the child's sample
/// value has the child's as an operand ([`operand_guarantee`]) and other
/// columns (labels, timestamps) are exact. `None` when the child has no
/// sample value or the column's guarantee is unknown.
pub fn project_column_guarantee(
    child: &Rc<OperatorNode>,
    column: usize,
) -> Option<AccuracyGuarantee> {
    // Over a join, each column's own guarantee ([`column_guarantees`]).
    if derives_from_join(child) {
        return column_guarantees(child)?.get(column).cloned();
    }
    let value = resolve_column_ref(&ColumnRef::SampleValue, &child.schema).ok()?;
    if column == value {
        operand_guarantee(child)
    } else {
        Some(AccuracyGuarantee::exact(
            "column other than the sample value",
        ))
    }
}

/// The guarantee of a scalar expression, given each column's: `scalar(v)`
/// and a SQL scalar subquery have their relation's, a literal or the
/// evaluation time is exact, arithmetic composes as
/// [`arithmetic_guarantee`], and `clamp`/`clamp_min`/`clamp_max`/`abs` as
/// [`lipschitz_guarantee`]. Any other expression (a comparison, a rounding,
/// a non-linear function) is exact over exact operands and has none
/// otherwise.
pub(crate) fn scalar_guarantee(
    expr: &ScalarExpr,
    column: &dyn Fn(usize) -> Option<AccuracyGuarantee>,
) -> Option<AccuracyGuarantee> {
    let literal = |expr: &ScalarExpr| match expr {
        ScalarExpr::Literal(ScalarValue::Float64(value)) => Some(*value),
        ScalarExpr::Literal(ScalarValue::Int64(value)) => Some(*value as f64),
        _ => None,
    };
    let operand = |expr: &ScalarExpr| (operand_of(expr, column), literal(expr));
    match expr {
        ScalarExpr::Column(id) => column(*id),
        ScalarExpr::PromqlScalarFromVector(node) | ScalarExpr::ScalarSubquery(node) => {
            node.guarantee.clone()
        }
        ScalarExpr::Literal(_) | ScalarExpr::CurrentTimestamp | ScalarExpr::EvalTimestamp => {
            Some(AccuracyGuarantee::exact("constant"))
        }
        ScalarExpr::Negative { expr, .. } => {
            lipschitz_guarantee(vec![scalar_guarantee(expr, column)?])
        }
        ScalarExpr::Case {
            operand,
            branches,
            else_expr,
        } => case_guarantee(operand.as_deref(), branches, else_expr.as_deref(), column),
        ScalarExpr::Arithmetic {
            op, left, right, ..
        } => arithmetic_guarantee(
            op.clone(),
            operand(left),
            operand(right),
            same_value(left, right),
        ),
        ScalarExpr::FunctionCall { name, args } if is_monotone_lipschitz(name) => {
            lipschitz_guarantee(
                args.iter()
                    .map(|arg| operand_of(arg, column))
                    .collect::<Option<_>>()?,
            )
        }
        _ => {
            let mut columns = Vec::new();
            expr.map_columns(&mut |id| {
                columns.push(id);
                id
            });
            let exact = |g: Option<AccuracyGuarantee>| g.is_some_and(|g| g.is_exact());
            (columns.into_iter().all(|id| exact(column(id)))
                && expr
                    .operator_refs()
                    .into_iter()
                    .all(|node| exact(node.guarantee.clone())))
            .then(|| AccuracyGuarantee::exact("exact over exact operands"))
        }
    }
}

/// The guarantee of a scalar operand as an input to arithmetic or a
/// function: a planned operator's as [`operand_guarantee`], anything else
/// as [`scalar_guarantee`].
pub fn operand_of(
    expr: &ScalarExpr,
    column: &dyn Fn(usize) -> Option<AccuracyGuarantee>,
) -> Option<AccuracyGuarantee> {
    match expr {
        ScalarExpr::PromqlScalarFromVector(node) | ScalarExpr::ScalarSubquery(node) => {
            operand_guarantee(node)
        }
        _ => scalar_guarantee(expr, column),
    }
}

/// `node`'s guarantee as an input to a computation over its values: a
/// Count-Min or Count Sketch bound restated as an absolute error over the
/// stream norm its summary's update weight allows ([`stream_norm`],
/// [`additive_as_absolute`]).
pub fn operand_guarantee(node: &Rc<OperatorNode>) -> Option<AccuracyGuarantee> {
    let guarantee = node.guarantee.clone()?;
    Some(additive_as_absolute(guarantee, stream_norm(node).as_ref()))
}

/// The stream norm that bounds the one Count-Min or Count Sketch summary
/// below `node`, by its update weight: the source's own norm when the
/// weight is the source's raw value column (the summary's input is the
/// source's scan, at most windowed or filtered); `|c|` times the source's
/// row count for a constant weight `c`. `None` for any other weight (a
/// computed value such as `m * 1000` or `rate(…)`), several summaries or
/// sources, or none.
fn stream_norm(node: &Rc<OperatorNode>) -> Option<StreamNorm> {
    let summaries: Vec<_> = OperatorNode::reachable(node)
        .into_iter()
        .filter(|node| {
            matches!(
                node.asap(),
                Some(ASAPOp::SummaryAgg {
                    family: FieldDataType::Sketch(kind, _),
                    ..
                }) if matches!(
                    kind.algorithm(),
                    SketchAlgorithm::Cms
                        | SketchAlgorithm::CmsWithHeap
                        | SketchAlgorithm::CountSketch
                        | SketchAlgorithm::CountSketchWithHeap
                )
            )
        })
        .collect();
    let [summary] = summaries.as_slice() else {
        return None;
    };
    let Some(ASAPOp::SummaryAgg {
        input: update,
        child,
        ..
    }) = summary.asap()
    else {
        return None;
    };
    let source = source(child)?;
    match &update.weight {
        SummaryInputExpr::Constant(weight) => Some(StreamNorm::ConstantWeight {
            weight: *weight,
            source,
        }),
        SummaryInputExpr::Column(column) if raw_value_column(child, column) => {
            Some(StreamNorm::Source(source))
        }
        _ => None,
    }
}

/// Whether `column` of `node` is its source's raw data sample value: `node` is
/// the source's scan, at most behind filters and windows that keep its
/// schema, and `column` resolves to the scan's sample value.
fn raw_value_column(node: &Rc<OperatorNode>, column: &ColumnRef) -> bool {
    let Some(schema) = source_scan(node) else {
        return false;
    };
    let resolved = resolve_column_ref(column, schema).ok();
    resolved.is_some() && resolved == resolve_column_ref(&ColumnRef::SampleValue, schema).ok()
}

/// The scan's schema when `node` is a source's scan, at most behind filters
/// and windows that keep its schema: its rows are the source's own.
fn source_scan(node: &Rc<OperatorNode>) -> Option<&Schema> {
    let mut current = node;
    loop {
        if current.schema != node.schema {
            return None;
        }
        match current.non_asap() {
            Some(NonASAPOp::Filter { child, .. } | NonASAPOp::TimeRange { child, .. }) => {
                current = child
            }
            Some(NonASAPOp::Scan { schema, .. }) => return Some(schema),
            _ => return None,
        }
    }
}

/// The one source (PromQL metric or SQL table) `node` reads; `None` when it
/// reads several or none.
fn source(node: &Rc<OperatorNode>) -> Option<String> {
    let sources: HashSet<String> = OperatorNode::reachable(node)
        .iter()
        .filter_map(|node| match node.non_asap() {
            Some(NonASAPOp::Scan { source, .. }) => Some(match source {
                Source::TimeSeries { metric } => metric.clone(),
                Source::Table { table_ref } => table_ref.clone(),
            }),
            _ => None,
        })
        .collect();
    let mut sources = sources.into_iter();
    match (sources.next(), sources.next()) {
        (Some(source), None) => Some(source),
        _ => None,
    }
}

/// Whether two scalar operands are one value: the same column, or the same
/// planned operator read twice.
fn same_value(left: &ScalarExpr, right: &ScalarExpr) -> bool {
    match (left, right) {
        (ScalarExpr::Column(a), ScalarExpr::Column(b)) => a == b,
        (
            ScalarExpr::PromqlScalarFromVector(a) | ScalarExpr::ScalarSubquery(a),
            ScalarExpr::PromqlScalarFromVector(b) | ScalarExpr::ScalarSubquery(b),
        ) => Rc::ptr_eq(a, b),
        _ => false,
    }
}

/// Functions that are 1-Lipschitz and, for the relative-error rule,
/// monotone in each argument with `|f(x)|` scaling with `|x|`: `clamp`
/// (`min(max(x, lo), hi)`), `clamp_min`, `clamp_max` and `abs`.
fn is_monotone_lipschitz(name: &str) -> bool {
    matches!(
        name,
        "promql_clamp" | "promql_clamp_min" | "promql_clamp_max" | "promql_abs" | "abs"
    )
}

/// The guarantee of a 1-Lipschitz function of `args` (negation, `abs`,
/// `clamp`): its value is within the largest of its arguments' absolute
/// errors, by the union bound over them; likewise for relative errors, as
/// for an exact `max`/`min` (`ExactExtremum`, one value per argument).
/// Count-Min and Count Sketch bounds count as absolute errors. `None` for a
/// rank error or a mix of absolute and relative ones.
fn lipschitz_guarantee(args: Vec<AccuracyGuarantee>) -> Option<AccuracyGuarantee> {
    let approximate: Vec<_> = args
        .into_iter()
        .filter(|g| !g.is_exact())
        .map(|g| additive_as_absolute(g, None))
        .collect();
    if approximate.is_empty() {
        return Some(AccuracyGuarantee::exact(
            "Lipschitz function of exact operands",
        ));
    }
    propagate(
        &CompositionOperator::ExactExtremum,
        &approximate,
        None,
        &PropagationStats {
            input_row_count: Some(1),
            ..PropagationStats::default()
        },
    )
    .ok()
}

/// An arithmetic operand: its guarantee, and its value when it is a literal.
type Operand = (Option<AccuracyGuarantee>, Option<f64>);

/// The guarantee of `left op right` (#669, MVP). Count-Min and Count Sketch
/// bounds count as absolute errors (normalized bound times stream norm).
/// - `x ± y`: absolute errors add (union bound on the failure
///   probabilities); `x ± c` (`c` exact) keeps an absolute error.
/// - `c · x`, `x / c` (`c` a nonzero literal): an absolute error scales by
///   `|c|` (or `1/|c|`); a relative error is kept, as it is by `x · y` or
///   `x / y` with `y` exact.
/// - `x · y` of two relative errors: relative `a + b + a·b`.
/// - `x · y` of two other estimates, `x / y` with `y` approximate: a bound
///   that needs enforced operand domains, left symbolic
///   ([`operand_domain_statistic`]) for plan selection to bind.
/// - Everything else (a relative error shifted by a constant or added to
///   another, a rank error, `%`, `^`, `atan2`) has none.
///
/// `same_operand`: both sides are one estimate (the same planned operator),
/// which fails or holds as one event, so its failure probability counts
/// once. Two estimates count once each, even over one shared summary: the
/// built-in guarantees hold per estimate, not simultaneously.
pub(crate) fn arithmetic_guarantee(
    op: ArithmeticOpKind,
    left: Operand,
    right: Operand,
    same_operand: bool,
) -> Option<AccuracyGuarantee> {
    let (l, r) = (
        additive_as_absolute(left.0?, None),
        additive_as_absolute(right.0?, None),
    );
    let once = same_operand.then(|| l.spec.failure_probability.clone());
    let statistic = operand_domain_statistic(&op, &l, &r);
    let composed = match (op, l.is_exact(), r.is_exact()) {
        (_, true, true) => return Some(AccuracyGuarantee::exact("arithmetic over exact operands")),
        (ArithmeticOpKind::Add | ArithmeticOpKind::Sub, true, false) => shifted(r),
        (ArithmeticOpKind::Add | ArithmeticOpKind::Sub, false, true) => shifted(l),
        (ArithmeticOpKind::Add | ArithmeticOpKind::Sub, false, false) => compose(
            &CompositionOperator::Lipschitz { constant: 1.0 },
            vec![l],
            Some(&r),
        ),
        (ArithmeticOpKind::Mul, true, false) => scaled(r, left.1.map(f64::abs)),
        (ArithmeticOpKind::Mul, false, true) => scaled(l, right.1.map(f64::abs)),
        (ArithmeticOpKind::Mul, false, false) if statistic.is_none() => relative_product(l, r),
        (ArithmeticOpKind::Div, false, true) => match right.1 {
            Some(0.0) => None,
            c => scaled(l, c.map(|c| 1.0 / c.abs())),
        },
        (ArithmeticOpKind::Mul | ArithmeticOpKind::Div, _, _) => symbolic(l, r, statistic?),
        _ => None,
    }?;
    let mut composed = composed;
    if let Some(failure_probability) = once {
        composed.spec.failure_probability = failure_probability;
    }
    Some(composed)
}

/// The statistic a product or quotient of `l` and `r` leaves symbolic on
/// its operands' domains, when it is one whose bound needs them: a product
/// of two estimates, other than of two relative errors
/// ([`PRODUCT_OPERAND_DOMAINS`]); a quotient with an approximate
/// denominator ([`DIVISION_OPERAND_DOMAINS`] when both operands are
/// relative or exact, [`QUOTIENT_OPERAND_DOMAINS`] otherwise). `None` for
/// any other operation, or when an operand's error has no value form (a
/// rank error). Plan selection (Stage 3) finds the operations to bind with
/// this same rule.
pub fn operand_domain_statistic(
    op: &ArithmeticOpKind,
    l: &AccuracyGuarantee,
    r: &AccuracyGuarantee,
) -> Option<&'static str> {
    let valued = |g: &AccuracyGuarantee| {
        g.is_exact()
            || matches!(
                g.spec.accuracy_metric,
                AccuracyMetric::ABSOLUTE | AccuracyMetric::Relative
            )
    };
    let relative =
        |g: &AccuracyGuarantee| g.is_exact() || g.spec.accuracy_metric == AccuracyMetric::Relative;
    if !(valued(l) && valued(r)) {
        return None;
    }
    match op {
        ArithmeticOpKind::Mul
            if !l.is_exact()
                && !r.is_exact()
                && !(l.spec.accuracy_metric == AccuracyMetric::Relative
                    && r.spec.accuracy_metric == AccuracyMetric::Relative) =>
        {
            Some(PRODUCT_OPERAND_DOMAINS)
        }
        ArithmeticOpKind::Div if !r.is_exact() => Some(if relative(l) && relative(r) {
            DIVISION_OPERAND_DOMAINS
        } else {
            QUOTIENT_OPERAND_DOMAINS
        }),
        _ => None,
    }
}

/// A product of two relative-error estimates: within relative
/// `(1 + a)(1 + b) − 1 = a + b + a·b`, whatever the signs. `None` for any
/// other pair.
fn relative_product(l: AccuracyGuarantee, r: AccuracyGuarantee) -> Option<AccuracyGuarantee> {
    if l.spec.accuracy_metric != AccuracyMetric::Relative
        || r.spec.accuracy_metric != AccuracyMetric::Relative
    {
        return None;
    }
    let bound = BoundExpr::Sum {
        terms: vec![
            l.spec.error.clone(),
            r.spec.error.clone(),
            BoundExpr::Product {
                factors: vec![l.spec.error.clone(), r.spec.error.clone()],
            },
        ],
    };
    Some(binary_guarantee(
        AccuracyMetric::Relative,
        bound,
        l,
        r,
        None,
        CompositionOperator::ExactProduct,
        "relative_product_union_bound",
    ))
}

/// `x` times an exact factor (`Some(|c|)` when it is a literal): a
/// relative error is kept; an absolute error scales by `|c|`.
fn scaled(input: AccuracyGuarantee, factor: Option<f64>) -> Option<AccuracyGuarantee> {
    match (input.spec.accuracy_metric, factor) {
        (AccuracyMetric::Relative, _) => Some(input),
        (_, Some(constant)) => compose(
            &CompositionOperator::Lipschitz { constant },
            vec![input],
            None,
        ),
        _ => None,
    }
}

/// `x` plus an exact value: an absolute error is kept.
fn shifted(input: AccuracyGuarantee) -> Option<AccuracyGuarantee> {
    compose(
        &CompositionOperator::Lipschitz { constant: 1.0 },
        vec![input],
        None,
    )
}

/// A product or quotient whose bound needs its operands' domains, which
/// Stage 1 does not read: the bound is the symbolic `statistic`
/// ([`operand_domain_statistic`]), relative for a ratio of relative errors
/// and absolute otherwise, with the union bound on the failure probability.
fn symbolic(
    l: AccuracyGuarantee,
    r: AccuracyGuarantee,
    statistic: &'static str,
) -> Option<AccuracyGuarantee> {
    let (metric, operator, rule) = match statistic {
        DIVISION_OPERAND_DOMAINS => (
            AccuracyMetric::Relative,
            CompositionOperator::ExactDivision,
            "relative_division_needs_operand_domains",
        ),
        QUOTIENT_OPERAND_DOMAINS => (
            AccuracyMetric::ABSOLUTE,
            CompositionOperator::ExactDivision,
            "absolute_division_needs_operand_domains",
        ),
        _ => (
            AccuracyMetric::ABSOLUTE,
            CompositionOperator::ExactProduct,
            "absolute_product_needs_operand_domains",
        ),
    };
    Some(binary_guarantee(
        metric,
        BoundExpr::Unknown {
            statistic: statistic.into(),
        },
        l,
        r,
        Some(statistic),
        operator,
        rule,
    ))
}

/// A guarantee over two operands with the union bound on their failure
/// probabilities, and its provenance.
fn binary_guarantee(
    metric: AccuracyMetric,
    bound: BoundExpr,
    l: AccuracyGuarantee,
    r: AccuracyGuarantee,
    unavailable: Option<&str>,
    operator: CompositionOperator,
    rule: &str,
) -> AccuracyGuarantee {
    let failure_probability = ProbabilityExpr::UnionBound {
        terms: vec![
            l.spec.failure_probability.clone(),
            r.spec.failure_probability.clone(),
        ],
    };
    let provenance = [l, r]
        .into_iter()
        .enumerate()
        .map(|(input_index, g)| GuaranteeSource::ChildGuarantee {
            input_index,
            guarantee: Box::new(g),
        })
        .chain(
            unavailable.map(|statistic| GuaranteeSource::UnavailableStatistic {
                statistic: statistic.into(),
            }),
        )
        .chain([GuaranteeSource::CompositionStep {
            operator,
            rule: rule.into(),
        }])
        .collect();
    AccuracyGuarantee::new(metric, bound, failure_probability, provenance)
}

/// Whether the default accuracy model's bound for `family` answers `query`
/// over `update`. A heap sketch's bound is per item frequency, not Top-K
/// membership; a Count-Min bound assumes non-negative weights.
fn estimate_has_bound(
    family: &FieldDataType,
    update: &SummaryUpdate,
    query: &SketchStatistic,
) -> bool {
    let count_min = matches!(
        family,
        FieldDataType::Sketch(kind, _)
            if matches!(kind.algorithm(), SketchAlgorithm::Cms | SketchAlgorithm::CmsWithHeap)
    );
    !matches!(query, SketchStatistic::TopK { .. })
        && !(count_min && !matches!(update.weight_domain, WeightDomain::NonNegative { .. }))
}

/// A Count-Min estimate whose weights carry the open requirement that they
/// are raw data samples of one metric ([`WeightDomain::RawDataSamplesOfMetric`]): its local
/// guarantee with the bound gated on that metric's sign
/// ([`raw_data_samples_sign_statistic`]), which plan selection binds to 1 when the
/// metric is declared a counter. `None` for any other estimate.
fn estimate_bound_over_raw_data_samples(
    family: &FieldDataType,
    update: &SummaryUpdate,
    query: &SketchStatistic,
) -> Option<AccuracyGuarantee> {
    let WeightDomain::RawDataSamplesOfMetric { metric } = &update.weight_domain else {
        return None;
    };
    let proven = SummaryUpdate {
        weight_domain: WeightDomain::NonNegative {
            proof: NonNegativeWeightProof::CounterRawDataSamples,
        },
        ..update.clone()
    };
    if !estimate_has_bound(family, &proven, query) {
        return None;
    }
    let mut local = local_guarantee(family, query)?;
    local.spec.error = asap_types::ir::properties::BoundExpr::Product {
        factors: vec![
            local.spec.error,
            asap_types::ir::properties::BoundExpr::Unknown {
                statistic: raw_data_samples_sign_statistic(metric),
            },
        ],
    };
    Some(local)
}

/// Replace an aggregate whose measures are `targets`' (or exact, for a
/// measure that is no target) by its substitution, with the same output
/// columns.
fn substitute(
    target: &Rc<OperatorNode>,
    targets: &[ChosenComputation<'_>],
    sizing: &Sizing,
    memo: &Memo,
) -> Result<Rc<OperatorNode>, PerComputationCandidateError> {
    let Some(NonASAPOp::Aggregate {
        child,
        reduction,
        measures,
        filters,
        ..
    }) = target.non_asap()
    else {
        return Err(PerComputationCandidateError::Unsupported(
            "target is not an aggregate",
        ));
    };
    // A key-only `HAVING` is a row predicate over the input: rows of a
    // dropped group never reach the state.
    let having = &targets[0].target.having;
    // Operator refs in a predicate (a subquery) are this aggregate's
    // children, already rewritten.
    let rewritten = |pred: &Predicate| {
        Predicate(
            pred.0
                .map_operator_refs(&mut |node| memo[&Rc::as_ptr(node)].clone()),
        )
    };
    let mut input = memo[&Rc::as_ptr(child)].clone();
    for entry in having {
        if entry.placement == HavingPlacement::BelowAggregate {
            let keys = reduction.group_keys().map_or(&[][..], |by| by.keys());
            let pred = Predicate(entry.predicate.0.map_columns(&mut |column| keys[column]));
            input = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Filter {
                pred,
                child: input,
            }))?;
        }
    }
    let parts = measures
        .iter()
        .enumerate()
        .map(|(measure, intent)| {
            // `fused`: the candidate is the target's fused computation's,
            // which absorbs the target beneath.
            let (candidate, raw_data_samples_of, fused) = targets
                .iter()
                .find(|chosen| chosen.target.measure == measure)
                .map_or((&ComputationCandidate::Exact, None, false), |chosen| {
                    (
                        chosen.candidate,
                        chosen.target.raw_data_samples_of.as_deref(),
                        chosen.fused,
                    )
                });
            let filter = filters.get(measure).and_then(Option::as_ref).map(rewritten);
            let key = (Rc::as_ptr(target), measure);
            if fused {
                return match candidate {
                    ComputationCandidate::ExactAccumulator(_) => nested_sum(target, memo),
                    ComputationCandidate::Summary(algorithm) => whole_expression(
                        target,
                        intent,
                        algorithm,
                        raw_data_samples_of,
                        (sizing, key),
                        memo,
                    ),
                    ComputationCandidate::Exact => Err(PerComputationCandidateError::Unsupported(
                        "plain exact execution of a fused computation",
                    )),
                };
            }
            substitute_measure(
                target,
                intent,
                candidate,
                filter,
                &input,
                raw_data_samples_of,
                (sizing, key),
            )
        })
        .collect::<Result<Vec<_>, _>>()?;
    let mut output = match <[_; 1]>::try_from(parts) {
        Ok([part]) => with_names_of(target, part)?,
        Err(parts) => join_measures(target, parts)?,
    };
    for entry in having {
        if entry.placement == HavingPlacement::AboveOutput && entry.filter.is_none() {
            output = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Filter {
                pred: rewritten(&entry.predicate),
                child: output,
            }))?;
        }
    }
    Ok(output)
}

/// `output` with `target`'s column names: an estimate or accumulator names
/// its value after the operation (`quantile`, `sum`), not the aggregate's
/// output column.
fn with_names_of(
    target: &OperatorNode,
    output: Rc<OperatorNode>,
) -> Result<Rc<OperatorNode>, PerComputationCandidateError> {
    let names = |node: &OperatorNode| -> Vec<String> {
        node.schema
            .fields
            .iter()
            .map(|field| field.name.clone())
            .collect()
    };
    // A top-k's ranked rows may carry their item columns after the
    // aggregate's ([`ranked_as_topk`]).
    if names(&output).starts_with(&names(target)) {
        return Ok(output);
    }
    if output.schema.fields.len() != target.schema.fields.len() {
        return Err(PerComputationCandidateError::Unsupported(
            "substituted output with other columns than the aggregate",
        ));
    }
    let cols = names(target)
        .into_iter()
        .enumerate()
        .map(|(column, name)| ProjectItem {
            alias: Some(name),
            expr: ScalarExpr::Column(column),
        })
        .collect();
    Ok(OperatorNode::new_shared(Operator::NonASAP(
        NonASAPOp::Project {
            cols,
            qualifier: None,
            child: output,
        },
    ))?)
}

/// A whole-expression top-k for `target` ([`whole_expression_input`]): one
/// heap summary of `algorithm` over the absorbed aggregate's rewritten
/// input, partitioned as `target`.
fn whole_expression(
    target: &OperatorNode,
    intent: &AggIntent,
    algorithm: &SketchAlgorithm,
    raw_data_samples_of: Option<&str>,
    (sizing, key): (&Sizing, MeasureKey),
    memo: &Memo,
) -> Result<Rc<OperatorNode>, PerComputationCandidateError> {
    let (input, update, partitions) = whole_expression_input(target).ok_or(
        PerComputationCandidateError::Unsupported("whole-expression top-k input"),
    )?;
    let state = OperatorNode::new_shared(Operator::ASAP(ASAPOp::SummaryAgg {
        child: memo[&Rc::as_ptr(&input)].clone(),
        input: with_raw_data_samples_requirement(update, raw_data_samples_of),
        family: FieldDataType::Sketch(
            sized(algorithm, intent, sizing.share(key)),
            GroupingStrategy::default(),
        ),
        reduction: partitions,
        grouping: GroupingStrategy::default(),
        filter: None,
    }))?;
    sizing.built.borrow_mut().push((state.clone(), key));
    let estimate = OperatorNode::new_shared(Operator::ASAP(ASAPOp::SummaryEstimate {
        summary_input: state,
        query: statistic(intent)?,
    }))?;
    ranked_as_topk(target, estimate)
}

/// A heap's top-k `estimate` (`[partitions.., items.., topk]`, one row per
/// ranked item) in `target`'s columns: its `[keys.., topk_k]`, with
/// `topk_k` the estimated Float64 weight, then, over PromQL rows, the item
/// columns. This is the exact top-k's physical shape (its ranked rows'
/// value, then their other labels; a SQL result keeps only its logical
/// columns), so readers of `[keys.., topk_k]` find those columns where they
/// were.
fn ranked_as_topk(
    target: &OperatorNode,
    estimate: Rc<OperatorNode>,
) -> Result<Rc<OperatorNode>, PerComputationCandidateError> {
    let (ranked, keys) = target
        .schema
        .fields
        .split_last()
        .expect("an aggregate outputs its measure");
    let fields = &estimate.schema.fields;
    let estimated = fields.len() - 1;
    let column = |index: usize, name: &str| ProjectItem {
        alias: Some(name.to_string()),
        expr: ScalarExpr::Column(index),
    };
    // Each key by name among the estimate's partition and item columns.
    let key_columns = keys
        .iter()
        .map(|key| {
            fields[..estimated]
                .iter()
                .position(|field| field.name == key.name)
                .ok_or(PerComputationCandidateError::Unsupported(
                    "top-k estimate without a partition column",
                ))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let promql = match target.non_asap() {
        Some(NonASAPOp::Aggregate { child, .. }) => reads_only_time_series(child),
        _ => false,
    };
    let cols = key_columns
        .iter()
        .zip(keys)
        .map(|(&index, key)| column(index, &key.name))
        .chain(std::iter::once(column(estimated, &ranked.name)))
        .chain(
            (0..estimated)
                .filter(|index| promql && !key_columns.contains(index))
                .map(|index| column(index, &fields[index].name)),
        )
        .collect();
    Ok(OperatorNode::new_shared(Operator::NonASAP(
        NonASAPOp::Project {
            cols,
            qualifier: None,
            child: estimate,
        },
    ))?)
}

/// A nested-sum merge for `target` ([`nested_sum_input`]): one exact `Sum`
/// accumulator over the absorbed sum's rewritten input, grouped as
/// `target`, finalized.
fn nested_sum(
    target: &OperatorNode,
    memo: &Memo,
) -> Result<Rc<OperatorNode>, PerComputationCandidateError> {
    let (rows, intent, reduction) = nested_sum_input(target).ok_or(
        PerComputationCandidateError::Unsupported("nested-sum merge input"),
    )?;
    let state = exact_accumulator(
        ExactKind::Sum,
        &intent,
        &memo[&Rc::as_ptr(&rows)],
        reduction,
    )?;
    Ok(OperatorNode::new_shared(Operator::ASAP(
        ASAPOp::FinalizeExactAccumulator { child: state },
    ))?)
}

/// An unfiltered exact accumulator of `kind` for `intent` over `input`,
/// grouped by `reduction`.
pub(crate) fn exact_accumulator(
    kind: ExactKind,
    intent: &AggIntent,
    input: &Rc<OperatorNode>,
    reduction: Reduction,
) -> Result<Rc<OperatorNode>, PerComputationCandidateError> {
    let family = exact_family(kind);
    let read = measure_input(intent, input, None)?;
    Ok(OperatorNode::new_shared(Operator::ASAP(
        ASAPOp::SummaryAgg {
            input: summary_update(intent, &family, &reduction, &input.schema, read)?,
            child: Rc::clone(input),
            family,
            reduction,
            grouping: GroupingStrategy::default(),
            filter: None,
        },
    ))?)
}

/// One measure of `target` over `input`: its group keys, then its value.
fn substitute_measure(
    target: &OperatorNode,
    intent: &AggIntent,
    candidate: &ComputationCandidate,
    filter: Option<Predicate>,
    input: &Rc<OperatorNode>,
    raw_data_samples_of: Option<&str>,
    (sizing, key): (&Sizing, MeasureKey),
) -> Result<Rc<OperatorNode>, PerComputationCandidateError> {
    let Some(NonASAPOp::Aggregate { reduction, .. }) = target.non_asap() else {
        unreachable!("substitute checked the aggregate")
    };
    let state = |family: FieldDataType, intent: &AggIntent, read: SummaryInputExpr| {
        Ok::<_, PerComputationCandidateError>(OperatorNode::new_shared(Operator::ASAP(
            ASAPOp::SummaryAgg {
                child: input.clone(),
                input: with_raw_data_samples_requirement(
                    summary_update(intent, &family, reduction, &input.schema, read)?,
                    raw_data_samples_of,
                ),
                family,
                reduction: reduction.clone(),
                grouping: GroupingStrategy::default(),
                filter: filter.clone(),
            },
        ))?)
    };
    let finalized = |kind: &ExactKind, intent: &AggIntent, read| {
        let state = state(exact_family(kind.clone()), intent, read)?;
        Ok::<_, PerComputationCandidateError>(OperatorNode::new_shared(Operator::ASAP(
            ASAPOp::FinalizeExactAccumulator { child: state },
        ))?)
    };
    Ok(match candidate {
        ComputationCandidate::Exact => {
            OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Aggregate {
                child: input.clone(),
                reduction: reduction.clone(),
                measures: vec![intent.clone()],
                output_names: vec![],
                filters: vec![filter.clone()],
                having: None,
            }))?
        }
        ComputationCandidate::ExactAccumulator(kinds) if kinds.len() == 1 => {
            let read = measure_input(intent, input, filter.as_ref())?;
            finalized(&kinds[0], intent, read)?
        }
        ComputationCandidate::Summary(algorithm) => {
            let family = FieldDataType::Sketch(
                sized(algorithm, intent, sizing.share(key)),
                GroupingStrategy::default(),
            );
            let summary_input = state(
                family,
                intent,
                measure_input(intent, input, filter.as_ref())?,
            )?;
            sizing.built.borrow_mut().push((summary_input.clone(), key));
            let estimate = OperatorNode::new_shared(Operator::ASAP(ASAPOp::SummaryEstimate {
                summary_input,
                query: statistic(intent)?,
            }))?;
            match intent {
                AggIntent::TopK { .. } => ranked_as_topk(target, estimate)?,
                _ => estimate,
            }
        }
        ComputationCandidate::ExactAccumulator(kinds) => {
            let (AggIntent::Avg { col }, [ExactKind::Sum, ExactKind::Count]) =
                (intent, kinds.as_slice())
            else {
                return Err(PerComputationCandidateError::Unsupported(
                    "several exact states other than avg's sum then count",
                ));
            };
            let sum_intent = AggIntent::Sum { col: *col };
            // The count counts the averaged column, as `avg` does: it is
            // non-null (a nullable input prunes this candidate), so it counts
            // the rows the sum reads.
            let read = measure_input(&sum_intent, input, filter.as_ref())?;
            let sum = finalized(&ExactKind::Sum, &sum_intent, read.clone())?;
            let count = finalized(
                &ExactKind::Count,
                &AggIntent::Count {
                    accuracy: AccuracyTarget::exact(),
                },
                read,
            )?;
            if input.result_kind == OperatorResultKind::Relation {
                return sql_avg(reduction, filter.is_some(), sum, count);
            }
            // `avg` is Float64 whatever the summed type, as `Avg`'s output is,
            // and a PromQL binary operator reads Float64 sample values. A
            // grouped sum or count ends with its value, cast when not Float64
            // (a count is Int64). A per-series sum keeps the input's labels,
            // so its value is the sample-value column; a per-series count is
            // Float64.
            let (sum, count) = match reduction {
                Reduction::Reduce(by) => (
                    last_as_float64(by.len(), sum)?,
                    last_as_float64(by.len(), count)?,
                ),
                Reduction::PerEntity => {
                    let value = resolve_column_ref(&ColumnRef::SampleValue, &sum.schema)
                        .ok()
                        .and_then(|column| sum.schema.fields.get(column));
                    if value.map(|field| &field.dtype)
                        != Some(&FieldDataType::Plain(DataType::Float64))
                    {
                        return Err(PerComputationCandidateError::Unsupported(
                            "non-Float64 per-series avg",
                        ));
                    }
                    (sum, count)
                }
            };
            OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::BinaryOp {
                operator: BinaryOperator {
                    checked_relative_division: false,
                    // A per-series window may hold no finite sum to divide.
                    checked_finite_division: matches!(reduction, Reduction::PerEntity),
                    kind: BinaryOpKind::Arithmetic(ArithmeticOpKind::Div),
                    vector_match: None,
                },
                return_bool: false,
                lhs: sum,
                rhs: count,
            }))?
        }
    })
}

/// `node`, a grouped aggregate's `keys` group keys and then its value, with
/// the value cast to Float64 unless it already is one.
fn last_as_float64(
    keys: usize,
    node: Rc<OperatorNode>,
) -> Result<Rc<OperatorNode>, PerComputationCandidateError> {
    if node.schema.fields.last().map(|field| &field.dtype)
        == Some(&FieldDataType::Plain(DataType::Float64))
    {
        return Ok(node);
    }
    let mut cols = key_items(keys);
    cols.push(ProjectItem {
        alias: None,
        expr: ScalarExpr::Cast {
            expr: Box::new(ScalarExpr::Column(keys)),
            to: DataType::Float64,
            try_cast: false,
        },
    });
    Ok(OperatorNode::new_shared(Operator::NonASAP(
        NonASAPOp::Project {
            cols,
            qualifier: None,
            child: node,
        },
    ))?)
}

/// SQL `avg` from its finalized `sum` and `count`: joined on the group keys
/// (NULL keys match), then `sum / count` as Float64. A group with no row
/// passing the measure filter, or an empty global input, counts 0 and its
/// `avg` is NULL.
fn sql_avg(
    reduction: &Reduction,
    filtered: bool,
    sum: Rc<OperatorNode>,
    count: Rc<OperatorNode>,
) -> Result<Rc<OperatorNode>, PerComputationCandidateError> {
    let Reduction::Reduce(by) = reduction else {
        return Err(PerComputationCandidateError::Unsupported(
            "SQL avg without `by` keys",
        ));
    };
    if by.is_without() {
        return Err(PerComputationCandidateError::Unsupported(
            "SQL avg without `by` keys",
        ));
    }
    let keys = by.len();
    let float = |column| {
        Box::new(ScalarExpr::Cast {
            expr: Box::new(ScalarExpr::Column(column)),
            to: DataType::Float64,
            try_cast: false,
        })
    };
    let count_column = keys + 1 + keys;
    let mut avg = ScalarExpr::Arithmetic {
        op: ArithmeticOpKind::Div,
        left: float(keys),
        right: float(count_column),
        semantics: ExprSemantics::Sql,
    };
    if filtered || keys == 0 {
        avg = ScalarExpr::Case {
            operand: None,
            branches: vec![(
                ScalarExpr::Compare {
                    left: Box::new(ScalarExpr::Column(count_column)),
                    op: CompareOpKind::Eq,
                    right: Box::new(ScalarExpr::Literal(ScalarValue::Int64(0))),
                    semantics: ExprSemantics::Sql,
                },
                ScalarExpr::Literal(ScalarValue::Null),
            )],
            else_expr: Some(Box::new(avg)),
        };
    }
    let mut cols = key_items(keys);
    cols.push(ProjectItem {
        alias: None,
        expr: avg,
    });
    Ok(OperatorNode::new_shared(Operator::NonASAP(
        NonASAPOp::Project {
            cols,
            qualifier: None,
            child: join_on_keys(keys, sum, count)?,
        },
    ))?)
}

/// `left` joined with `right` on their leading `keys` columns; NULL keys
/// match each other, as grouping treats them.
fn join_on_keys(
    keys: usize,
    left: Rc<OperatorNode>,
    right: Rc<OperatorNode>,
) -> Result<Rc<OperatorNode>, PerComputationCandidateError> {
    let width = left.schema.fields.len();
    let column = |id| Box::new(ScalarExpr::Column(id));
    let matches = (0..keys)
        .map(|key| {
            let equal = ScalarExpr::Compare {
                left: column(key),
                op: CompareOpKind::Eq,
                right: column(width + key),
                semantics: ExprSemantics::Sql,
            };
            if right.schema.fields[key].nullable {
                ScalarExpr::BoolOr(vec![
                    equal,
                    ScalarExpr::BoolAnd(vec![
                        ScalarExpr::IsNull(column(key)),
                        ScalarExpr::IsNull(column(width + key)),
                    ]),
                ])
            } else {
                equal
            }
        })
        .collect::<Vec<_>>();
    let (kind, pred) = match matches.len() {
        0 => (
            JoinKind::Cross,
            ScalarExpr::Literal(ScalarValue::Boolean(true)),
        ),
        _ => (JoinKind::Inner, ScalarExpr::BoolAnd(matches)),
    };
    Ok(OperatorNode::new_shared(Operator::NonASAP(
        NonASAPOp::Join {
            kind,
            pred: Predicate(pred),
            left,
            right,
        },
    ))?)
}

/// Join single-measure `parts` (each: group keys, then one value) on their
/// group keys and project `target`'s output columns. Every part has the same
/// groups, so an inner join keeps them all; NULL keys match each other, as
/// grouping treats them.
fn join_measures(
    target: &OperatorNode,
    parts: Vec<Rc<OperatorNode>>,
) -> Result<Rc<OperatorNode>, PerComputationCandidateError> {
    let Some(NonASAPOp::Aggregate {
        reduction: Reduction::Reduce(by),
        ..
    }) = target.non_asap()
    else {
        return Err(PerComputationCandidateError::Unsupported(
            "multi-measure aggregate without `by` keys",
        ));
    };
    if by.is_without() {
        return Err(PerComputationCandidateError::Unsupported(
            "multi-measure aggregate without `by` keys",
        ));
    }
    let keys = by.len();
    let width = keys + 1;
    let mut parts = parts.into_iter();
    let mut joined = parts.next().expect("an aggregate has measures");
    for part in parts {
        joined = join_on_keys(keys, joined, part)?;
    }
    let mut cols = key_items(keys);
    cols.extend(
        (0..target.schema.fields.len() - keys).map(|measure| ProjectItem {
            alias: None,
            expr: ScalarExpr::Column(measure * width + keys),
        }),
    );
    for (item, field) in cols.iter_mut().zip(&target.schema.fields) {
        item.alias = Some(field.name.clone());
    }
    Ok(OperatorNode::new_shared(Operator::NonASAP(
        NonASAPOp::Project {
            cols,
            qualifier: None,
            child: joined,
        },
    ))?)
}

fn key_items(keys: usize) -> Vec<ProjectItem> {
    (0..keys)
        .map(|key| ProjectItem {
            alias: None,
            expr: ScalarExpr::Column(key),
        })
        .collect()
}

impl Sizing {
    /// How many approximate inputs `key`'s summary shares its query's
    /// budget with.
    fn share(&self, key: MeasureKey) -> usize {
        self.shares.get(&key).copied().unwrap_or(1)
    }
}

/// `algorithm` sized for one of `share` equal shares of `intent`'s accuracy
/// requirement ([`per_input_target`]), split in the metric of the summary's own
/// bound: a relative error splits multiplicatively.
fn sized(algorithm: &SketchAlgorithm, intent: &AggIntent, share: usize) -> SketchKind {
    let target = sizing_target(intent).unwrap_or_else(AccuracyTarget::exact);
    let size = |target: &AccuracyTarget| {
        let (epsilon, delta) = accuracy_budget(target);
        SketchKind::new(
            algorithm.clone(),
            default_size_params(algorithm.clone(), intent, epsilon, delta),
        )
    };
    let whole = size(&target);
    if share <= 1 {
        return whole;
    }
    let metric = statistic(intent)
        .ok()
        .and_then(|query| {
            local_guarantee(
                &FieldDataType::Sketch(whole.clone(), GroupingStrategy::default()),
                &query,
            )
        })
        .map(|guarantee| guarantee.spec.accuracy_metric)
        .or(target.accuracy_metric);
    // The split needs a concrete metric. A summary without an error model
    // whose intent's target sets none is not split (never a guessed
    // metric): it stays sized for the whole target, and Stage 3 rejects the
    // candidate if its composed output then misses the query's target.
    let Some(metric) = metric else {
        debug_assert!(
            target.is_exact(),
            "sizing {algorithm:?} for a share of an approximate target without a metric"
        );
        return whole;
    };
    per_input_target(&target, metric, share).map_or(whole, |share| size(&share))
}

/// What a measure's state reads from each row of `input`, resolvable there.
/// A SQL `count` reads [`counted_column`]; any other measure, a PromQL count
/// included, reads its input column (the sample value when it names none).
fn measure_input(
    intent: &AggIntent,
    input: &OperatorNode,
    filter: Option<&Predicate>,
) -> Result<SummaryInputExpr, PerComputationCandidateError> {
    let schema = &input.schema;
    let read = match intent {
        AggIntent::Count { .. } if input.result_kind == OperatorResultKind::Relation => {
            SummaryInputExpr::Column(counted_column(schema, filter)?)
        }
        // A top-k ranks its child's aggregate output, not a column of its own.
        AggIntent::TopK { .. } => SummaryInputExpr::Column(
            crate::realization::ranked_value(input)
                .map_err(|_| {
                    PerComputationCandidateError::Unsupported("Top-K has no ranked column")
                })?
                .1,
        ),
        _ => crate::realization::summarised_input(intent, schema).ok_or(
            PerComputationCandidateError::Unsupported("input column outside child schema"),
        )?,
    };
    if let SummaryInputExpr::Column(column) = &read {
        resolve_column_ref(column, schema).map_err(|_| {
            PerComputationCandidateError::Unsupported("summary input column does not resolve")
        })?;
    }
    Ok(read)
}

/// The column a SQL `count` state updates on. The runtime skips NULL updates,
/// so it is a column non-null on every counted row: one the measure filter
/// proves non-null (`count(col)` lowers to the filter `col IS NOT NULL`), else
/// a non-nullable one, Float64 first (the runtime's numeric update).
fn counted_column(
    schema: &Schema,
    filter: Option<&Predicate>,
) -> Result<ColumnRef, PerComputationCandidateError> {
    fn not_null(expr: &ScalarExpr) -> Vec<usize> {
        match expr {
            ScalarExpr::IsNotNull(inner) => match **inner {
                ScalarExpr::Column(column) => vec![column],
                _ => vec![],
            },
            ScalarExpr::BoolAnd(conjuncts) => conjuncts.iter().flat_map(not_null).collect(),
            _ => vec![],
        }
    }
    let non_nullable = |float: bool| {
        schema
            .fields
            .iter()
            .enumerate()
            .filter_map(move |(id, field)| {
                (!field.nullable
                    && (!float || field.dtype == FieldDataType::Plain(DataType::Float64)))
                .then_some(id)
            })
    };
    let reference = |id: usize| schema.fields.get(id).map(crate::realization::column_ref);
    filter
        .map_or_else(Vec::new, |filter| not_null(&filter.0))
        .into_iter()
        .chain(non_nullable(true))
        .chain(non_nullable(false))
        .find_map(|id| {
            reference(id).filter(|column| resolve_column_ref(column, schema).ok() == Some(id))
        })
        .ok_or(PerComputationCandidateError::Unsupported(
            "SQL count over rows with no column known to be non-null",
        ))
}

/// What each input row contributes, given what the measure reads
/// ([`measure_input`]), following the legacy realization rules: heap sketches
/// rank series identities, frequency sketches count values, and every other
/// family reads the measure's input.
fn summary_update(
    intent: &AggIntent,
    family: &FieldDataType,
    reduction: &Reduction,
    child: &Schema,
    weight: SummaryInputExpr,
) -> Result<SummaryUpdate, PerComputationCandidateError> {
    let algorithm = match family {
        FieldDataType::Sketch(kind, _) => Some(kind.algorithm()),
        _ => None,
    };
    Ok(match (intent, algorithm) {
        (AggIntent::TopK { .. }, Some(_)) => {
            if reduction.group_keys().is_some_and(|keys| keys.is_without()) {
                return Err(PerComputationCandidateError::Unsupported(
                    "Top-K partitions given by `without`",
                ));
            }
            let groups: Vec<usize> = reduction
                .group_keys()
                .into_iter()
                .flat_map(|keys| keys.iter().copied())
                .collect();
            let item = if child.has_promql_series_identity() {
                // Rows that carry the full series identity rank it as a
                // column, the item form the runtime builds keyed summaries
                // from.
                SummaryInputExpr::Column(ColumnRef::Named(
                    asap_types::ir::schema::PROMQL_SERIES_IDENTITY.into(),
                ))
            } else if child.closed {
                // A closed child (an aggregate's output, say) lists every
                // column: the item is its columns outside the partition and
                // the ranked value, as the legacy current-series and counter
                // rules rank them.
                let SummaryInputExpr::Column(ranked) = &weight else {
                    return Err(PerComputationCandidateError::Unsupported(
                        "Top-K ranked value is not a column",
                    ));
                };
                let value = resolve_column_ref(ranked, child).map_err(|_| {
                    PerComputationCandidateError::Unsupported("Top-K ranked value does not resolve")
                })?;
                let items: Vec<_> = child
                    .fields
                    .iter()
                    .enumerate()
                    .filter(|(index, _)| *index != value && !groups.contains(index))
                    .map(|(_, field)| {
                        SummaryInputExpr::Column(crate::realization::column_ref(field))
                    })
                    .collect();
                if items.is_empty() {
                    return Err(PerComputationCandidateError::Unsupported(
                        "Top-K item has no label outside its partition",
                    ));
                }
                SummaryInputExpr::Tuple(items)
            } else {
                let excluding = groups
                    .iter()
                    .filter_map(|&index| child.fields.get(index))
                    .map(crate::realization::column_ref)
                    .collect();
                SummaryInputExpr::EntityIdentity(EntityIdentity::PromqlLabelSet { excluding })
            };
            SummaryUpdate {
                item: Some(item),
                // The ranked value ([`measure_input`]).
                weight,
                // Not proven non-negative; selection decides whether CMS is admissible.
                weight_domain: WeightDomain::UnknownOrSigned,
            }
        }
        (_, Some(SketchAlgorithm::UnivMon))
        | (AggIntent::Count { .. }, Some(SketchAlgorithm::Cms | SketchAlgorithm::CountSketch)) => {
            SummaryUpdate {
                item: Some(weight),
                weight: SummaryInputExpr::Constant(1.0),
                weight_domain: WeightDomain::NonNegative {
                    proof: NonNegativeWeightProof::UnitCount,
                },
            }
        }
        _ => SummaryUpdate {
            item: None,
            weight,
            weight_domain: WeightDomain::UnknownOrSigned,
        },
    })
}

fn statistic(intent: &AggIntent) -> Result<SketchStatistic, PerComputationCandidateError> {
    Ok(match intent {
        AggIntent::Quantile { q, .. } => SketchStatistic::Quantile { q: *q },
        AggIntent::Cardinality { .. } => SketchStatistic::Cardinality,
        AggIntent::FrequencyL2 { .. } => SketchStatistic::FrequencyL2,
        AggIntent::FrequencyEntropy { .. } => SketchStatistic::FrequencyEntropy,
        AggIntent::TopK { k, .. } => SketchStatistic::TopK { k: *k },
        AggIntent::Count { .. } => SketchStatistic::PointCount {
            key: ColumnRef::SampleValue,
            value: None,
        },
        _ => {
            return Err(PerComputationCandidateError::Unsupported(
                "sketch evaluation",
            ))
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use asap_types::ir::properties::AccuracyTarget;
    use asap_types::ir::schema::StatisticKind;
    /// Approximate count keeps one exact option, its accumulator, beside the
    /// universal summary; plain exact execution is not offered.
    #[test]
    fn approximate_count_keeps_one_exact_accumulator_and_universal_choices() {
        let choices = computation_candidates_for_intent(&AggIntent::Count {
            accuracy: AccuracyTarget::max_error(0.05).failure_probability(0.01),
        })
        .unwrap()
        .candidates;
        assert!(!choices.contains(&ComputationCandidate::Exact));
        assert_eq!(
            choices[0],
            ComputationCandidate::ExactAccumulator(vec![ExactKind::Count])
        );
        assert!(choices.contains(&ComputationCandidate::Summary(SketchAlgorithm::UnivMon)));
    }

    /// Guarantee composition through scalar and vector arithmetic (#669).
    mod guarantee_composition {
        use super::*;
        use crate::test_support::lower_promql;
        use asap_types::ir::properties::AccuracyTarget;

        fn with_bound(metric: AccuracyMetric, bound: f64, delta: f64) -> AccuracyGuarantee {
            AccuracyGuarantee::new(
                metric,
                BoundExpr::Constant { value: bound },
                ProbabilityExpr::Constant { value: delta },
                vec![],
            )
        }
        fn abs(bound: f64, delta: f64) -> AccuracyGuarantee {
            with_bound(AccuracyMetric::ABSOLUTE, bound, delta)
        }
        fn rel(bound: f64) -> AccuracyGuarantee {
            with_bound(AccuracyMetric::Relative, bound, 0.0)
        }
        /// `sum(m)` carrying `guarantee`.
        fn node(guarantee: AccuracyGuarantee) -> Rc<OperatorNode> {
            node_over("sum(m)", guarantee)
        }
        /// `query` carrying `guarantee`.
        fn node_over(query: &str, guarantee: AccuracyGuarantee) -> Rc<OperatorNode> {
            let node = lower_promql(query, AccuracyTarget::exact());
            Rc::new((*node).clone().with_guarantee(Some(guarantee)))
        }
        fn scalar(guarantee: AccuracyGuarantee) -> ScalarExpr {
            ScalarExpr::PromqlScalarFromVector(node(guarantee))
        }
        fn lit(value: f64) -> ScalarExpr {
            ScalarExpr::literal_f64(value)
        }
        fn arith(op: ArithmeticOpKind, left: ScalarExpr, right: ScalarExpr) -> ScalarExpr {
            ScalarExpr::Arithmetic {
                op,
                left: Box::new(left),
                right: Box::new(right),
                semantics: ExprSemantics::Promql,
            }
        }
        fn call(name: &str, args: Vec<ScalarExpr>) -> ScalarExpr {
            ScalarExpr::FunctionCall {
                name: name.into(),
                args,
            }
        }
        fn of(expr: &ScalarExpr) -> Option<AccuracyGuarantee> {
            scalar_guarantee(expr, &|_| None)
        }
        fn bound(g: &AccuracyGuarantee) -> f64 {
            g.spec.error.evaluate().expect("a resolved bound")
        }
        fn delta(g: &AccuracyGuarantee) -> f64 {
            g.spec
                .failure_probability
                .evaluate()
                .expect("a resolved probability")
        }
        fn close(a: f64, b: f64) -> bool {
            (a - b).abs() < 1e-12
        }

        /// Absolute errors add through `x ± y`, with the union bound on the
        /// failure probability; `−x` and `x ± c` keep the error, `c · x` and
        /// `x / c` scale it by `|c|` and `1/|c|`, division by a literal zero
        /// has none.
        #[test]
        fn absolute_errors_compose_through_arithmetic() {
            use ArithmeticOpKind::*;
            let (x, y) = (scalar(abs(0.01, 0.001)), scalar(abs(0.02, 0.002)));
            for op in [Add, Sub] {
                let g = of(&arith(op.clone(), x.clone(), y.clone())).unwrap();
                assert_eq!(g.spec.accuracy_metric, AccuracyMetric::ABSOLUTE, "{op:?}");
                assert!(close(bound(&g), 0.03) && close(delta(&g), 0.003), "{op:?}");
                for shifted in [
                    arith(op.clone(), x.clone(), lit(5.0)),
                    arith(op.clone(), lit(5.0), x.clone()),
                    arith(op.clone(), x.clone(), ScalarExpr::EvalTimestamp),
                ] {
                    let g = of(&shifted).unwrap();
                    assert!(close(bound(&g), 0.01) && close(delta(&g), 0.001), "{op:?}");
                }
            }
            let negated = ScalarExpr::Negative {
                expr: Box::new(x.clone()),
                semantics: ExprSemantics::Promql,
            };
            assert!(close(bound(&of(&negated).unwrap()), 0.01));
            for (expr, expected) in [
                (arith(Mul, lit(-3.0), x.clone()), 0.03),
                (arith(Mul, x.clone(), lit(3.0)), 0.03),
                (arith(Div, x.clone(), lit(-4.0)), 0.0025),
            ] {
                let g = of(&expr).unwrap();
                assert!(
                    close(bound(&g), expected) && close(delta(&g), 0.001),
                    "{expr:?}"
                );
            }
            for none in [
                arith(Div, x.clone(), lit(0.0)),
                arith(Mul, x.clone(), ScalarExpr::EvalTimestamp),
                arith(Pow, x.clone(), lit(2.0)),
            ] {
                assert_eq!(of(&none), None, "{none:?}");
            }
        }

        /// Count-Min and Count Sketch errors are additive: through
        /// arithmetic they compose as absolute errors, each normalized bound
        /// times the (symbolic) stream norm of its summary's source, here
        /// over each source's raw value column, for plan selection to bind.
        #[test]
        fn normalized_additive_errors_compose_as_absolute_errors() {
            let cms = with_bound(AccuracyMetric::ABSOLUTE_STREAM_L1, 0.01, 0.001);
            let count_sketch = with_bound(AccuracyMetric::ABSOLUTE_STREAM_L2, 0.02, 0.002);
            // Summaries of each source's raw value column.
            let raw = |query: &str, guarantee: AccuracyGuarantee| {
                ScalarExpr::PromqlScalarFromVector(crate::test_support::estimate_over(
                    crate::test_support::weighted(
                        crate::test_support::cms_summary(query),
                        SummaryInputExpr::Column(ColumnRef::SampleValue),
                    ),
                    guarantee,
                ))
            };
            let g = of(&arith(
                ArithmeticOpKind::Sub,
                raw("count by (job) (m)", cms.clone()),
                raw("count by (job) (n)", count_sketch),
            ))
            .unwrap();
            assert_eq!(g.spec.accuracy_metric, AccuracyMetric::ABSOLUTE);
            assert!(close(delta(&g), 0.003));
            let norms = |statistic: &str| match statistic {
                "stream_l1_norm[m]" => Some(100.0),
                "stream_l2_norm[n]" => Some(10.0),
                _ => None,
            };
            assert!(g.spec.error.has_unknown());
            assert!(close(g.bind(&norms).spec.error.evaluate().unwrap(), 1.2));
            let doubled = of(&arith(
                ArithmeticOpKind::Mul,
                lit(2.0),
                raw("count by (job) (m)", cms),
            ))
            .unwrap();
            assert!(close(
                doubled.bind(&norms).spec.error.evaluate().unwrap(),
                2.0
            ));
        }

        /// A product of two estimates has an absolute bound
        /// `|x|·e_y + |y|·e_x + e_x·e_y`, and a quotient with an approximate
        /// denominator one that needs the denominator bounded away from
        /// zero: both need operand domains, so Stage 1 leaves them symbolic
        /// for plan selection to bind; a ratio of relative errors keeps its
        /// relative form. A product of relative errors has the closed form
        /// `(1 + a)(1 + b) − 1`. The failure probability is the union bound.
        /// A rank error composes to none.
        #[test]
        fn products_and_quotients_of_estimates_have_symbolic_bounds() {
            use ArithmeticOpKind::*;
            let unknown = |statistic: &str| BoundExpr::Unknown {
                statistic: statistic.into(),
            };
            let (x, y) = (scalar(abs(0.01, 0.001)), scalar(abs(0.02, 0.002)));
            let product = of(&arith(Mul, x.clone(), y.clone())).unwrap();
            assert_eq!(product.spec.accuracy_metric, AccuracyMetric::ABSOLUTE);
            assert_eq!(product.spec.error, unknown(PRODUCT_OPERAND_DOMAINS));
            assert!(close(delta(&product), 0.003));
            for quotient in [
                arith(Div, x.clone(), y.clone()),
                arith(Div, lit(1.0), x.clone()),
                arith(Div, x.clone(), scalar(rel(0.01))),
            ] {
                let g = of(&quotient).unwrap();
                assert_eq!(
                    g.spec.accuracy_metric,
                    AccuracyMetric::ABSOLUTE,
                    "{quotient:?}"
                );
                assert_eq!(
                    g.spec.error,
                    unknown(QUOTIENT_OPERAND_DOMAINS),
                    "{quotient:?}"
                );
            }
            let ratio = of(&arith(Div, scalar(rel(0.01)), scalar(rel(0.02)))).unwrap();
            assert_eq!(ratio.spec.accuracy_metric, AccuracyMetric::Relative);
            assert_eq!(ratio.spec.error, unknown(DIVISION_OPERAND_DOMAINS));
            let relative = of(&arith(Mul, scalar(rel(0.01)), scalar(rel(0.02)))).unwrap();
            assert_eq!(relative.spec.accuracy_metric, AccuracyMetric::Relative);
            assert!(close(bound(&relative), 0.01 + 0.02 + 0.0002));
            let rank = scalar(with_bound(AccuracyMetric::Rank, 0.01, 0.001));
            assert_eq!(of(&arith(Mul, rank.clone(), x.clone())), None);
            assert_eq!(of(&arith(Div, x, rank)), None);
        }

        /// A relative error survives negation and scaling by an exact value,
        /// but not a shift (`x + c`) or a sum or difference of estimates
        /// (cancellation); a rank error composes to no guarantee (a rank
        /// interval counts as none in the MVP).
        #[test]
        fn relative_and_rank_errors_compose_only_where_sound() {
            use ArithmeticOpKind::*;
            let x = scalar(rel(0.01));
            for kept in [
                ScalarExpr::Negative {
                    expr: Box::new(x.clone()),
                    semantics: ExprSemantics::Promql,
                },
                arith(Mul, x.clone(), lit(-2.0)),
                arith(Div, x.clone(), ScalarExpr::EvalTimestamp),
            ] {
                let g = of(&kept).unwrap();
                assert_eq!(g.spec.accuracy_metric, AccuracyMetric::Relative, "{kept:?}");
                assert!(close(bound(&g), 0.01), "{kept:?}");
            }
            for none in [
                arith(Add, x.clone(), lit(1.0)),
                arith(Sub, x.clone(), scalar(rel(0.01))),
            ] {
                assert_eq!(of(&none), None, "{none:?}");
            }
            let rank = scalar(with_bound(AccuracyMetric::Rank, 0.01, 0.001));
            for none in [
                arith(Sub, rank.clone(), rank.clone()),
                arith(Mul, rank.clone(), lit(2.0)),
                call("promql_clamp_max", vec![rank, lit(1.0)]),
            ] {
                assert_eq!(of(&none), None, "{none:?}");
            }
        }

        /// `clamp`, `clamp_min`, `clamp_max` and `abs` are 1-Lipschitz and
        /// monotone in each argument: the result keeps the largest absolute
        /// (or relative) error of its arguments, with the union bound on the
        /// failure probability.
        #[test]
        fn clamp_and_abs_keep_the_largest_argument_error() {
            let x = scalar(abs(0.01, 0.001));
            for expr in [
                call("promql_clamp", vec![x.clone(), lit(0.0), lit(10.0)]),
                call("promql_clamp_min", vec![x.clone(), lit(0.0)]),
                call("promql_clamp_max", vec![x.clone(), lit(10.0)]),
                call("promql_abs", vec![x.clone()]),
                call("abs", vec![x.clone()]),
            ] {
                let g = of(&expr).unwrap();
                assert_eq!(g.spec.accuracy_metric, AccuracyMetric::ABSOLUTE, "{expr:?}");
                assert!(
                    close(bound(&g), 0.01) && close(delta(&g), 0.001),
                    "{expr:?}"
                );
            }
            let bounded = of(&call(
                "promql_clamp",
                vec![x.clone(), scalar(abs(0.03, 0.002)), lit(10.0)],
            ))
            .unwrap();
            assert!(close(bound(&bounded), 0.03) && close(delta(&bounded), 0.003));
            let relative = of(&call(
                "promql_clamp",
                vec![scalar(rel(0.01)), scalar(rel(0.02)), lit(10.0)],
            ))
            .unwrap();
            assert_eq!(relative.spec.accuracy_metric, AccuracyMetric::Relative);
            assert!(close(bound(&relative), 0.02));
            let mixed = call("promql_clamp_min", vec![x, scalar(rel(0.02))]);
            assert_eq!(of(&mixed), None);
            assert_eq!(of(&call("promql_floor", vec![scalar(rel(0.01))])), None);
        }

        /// A comparison over an estimate has no guarantee on its output (a
        /// band guarantee is later work); over exact operands it is exact.
        #[test]
        fn comparisons_over_estimates_have_no_guarantee() {
            let compare = |left: ScalarExpr| ScalarExpr::Compare {
                left: Box::new(left),
                op: CompareOpKind::Gt,
                right: Box::new(lit(5.0)),
                semantics: ExprSemantics::Promql,
            };
            assert_eq!(of(&compare(scalar(abs(0.01, 0.001)))), None);
            assert!(of(&compare(scalar(AccuracyGuarantee::exact("exact"))))
                .is_some_and(|g| g.is_exact()));
        }

        /// A SQL scalar subquery inherits its relation's guarantee, as
        /// PromQL `scalar(v)` does.
        #[test]
        fn a_scalar_subquery_inherits_its_guarantee() {
            let g = abs(0.01, 0.001);
            let expr = arith(
                ArithmeticOpKind::Add,
                ScalarExpr::ScalarSubquery(node(g.clone())),
                lit(1.0),
            );
            assert!(close(bound(&of(&expr).unwrap()), 0.01));
        }

        /// Two estimates count their failure probabilities once each (union
        /// bound), even when they read one shared summary: the built-in
        /// guarantees hold per estimate, not simultaneously. The same
        /// planned estimate read twice fails or holds as one event, so its
        /// failure probability counts once.
        #[test]
        fn failure_probability_counts_once_per_distinct_estimate() {
            let shared = node(abs(0.01, 0.001));
            // Equal to `shared`, but planned separately.
            let other = node(abs(0.01, 0.001));
            let same = ScalarExpr::PromqlScalarFromVector(shared.clone());
            let twice = of(&arith(ArithmeticOpKind::Add, same.clone(), same.clone())).unwrap();
            assert!(close(bound(&twice), 0.02) && close(delta(&twice), 0.001));
            let distinct = of(&arith(
                ArithmeticOpKind::Add,
                same,
                ScalarExpr::PromqlScalarFromVector(other.clone()),
            ))
            .unwrap();
            assert!(close(bound(&distinct), 0.02) && close(delta(&distinct), 0.002));
            let vector = |lhs: &Rc<OperatorNode>, rhs: &Rc<OperatorNode>| {
                let node = OperatorNode::new(Operator::NonASAP(NonASAPOp::BinaryOp {
                    operator: BinaryOperator {
                        checked_relative_division: false,
                        checked_finite_division: false,
                        kind: BinaryOpKind::Arithmetic(ArithmeticOpKind::Sub),
                        vector_match: None,
                    },
                    return_bool: false,
                    lhs: lhs.clone(),
                    rhs: rhs.clone(),
                }))
                .unwrap();
                composed_guarantee(&node).unwrap()
            };
            assert!(close(delta(&vector(&shared, &shared)), 0.001));
            assert!(close(delta(&vector(&shared, &other)), 0.002));
        }

        /// `left JOIN right ON pred`, composed.
        fn join(
            kind: JoinKind,
            left: &Rc<OperatorNode>,
            right: &Rc<OperatorNode>,
            pred: ScalarExpr,
        ) -> OperatorNode {
            let mut node = OperatorNode::new(Operator::NonASAP(NonASAPOp::Join {
                kind,
                pred: Predicate(pred),
                left: left.clone(),
                right: right.clone(),
            }))
            .unwrap();
            node.guarantee = composed_guarantee(&node);
            node
        }
        fn column(id: usize) -> ScalarExpr {
            ScalarExpr::Column(id)
        }
        fn equals(left: ScalarExpr, right: ScalarExpr) -> ScalarExpr {
            ScalarExpr::Compare {
                left: Box::new(left),
                op: CompareOpKind::Eq,
                right: Box::new(right),
                semantics: ExprSemantics::Sql,
            }
        }
        /// `CASE WHEN condition THEN NULL ELSE otherwise END` over `child`,
        /// composed.
        fn case_over(
            child: &OperatorNode,
            condition: ScalarExpr,
            otherwise: ScalarExpr,
        ) -> Option<AccuracyGuarantee> {
            let node = OperatorNode::new(Operator::NonASAP(NonASAPOp::Project {
                cols: vec![ProjectItem {
                    alias: Some("v".into()),
                    expr: ScalarExpr::Case {
                        operand: None,
                        branches: vec![(condition, ScalarExpr::Literal(ScalarValue::Null))],
                        else_expr: Some(Box::new(otherwise)),
                    },
                }],
                qualifier: None,
                child: Rc::new(child.clone()),
            }))
            .unwrap();
            composed_guarantee(&node)
        }
        /// An approximate node and an exact one, and the approximate one's
        /// column count (its columns come first in a join).
        fn approximate_and_exact() -> (Rc<OperatorNode>, Rc<OperatorNode>, usize) {
            let approximate = node(rel(0.01));
            let exact = node_over("count(m)", AccuracyGuarantee::exact("test"));
            let width = approximate.schema.fields.len();
            (approximate, exact, width)
        }

        /// A join whose other input is exact and whose predicate reads only
        /// exact columns keeps the approximate input's guarantee: a cross
        /// join, or a key equality on the exact input's columns.
        #[test]
        fn join_beside_an_exact_input_on_exact_columns_keeps_the_guarantee() {
            let (approximate, exact, width) = approximate_and_exact();
            for (kind, pred) in [
                (
                    JoinKind::Cross,
                    ScalarExpr::Literal(ScalarValue::Boolean(true)),
                ),
                (JoinKind::Inner, equals(column(width), lit(0.0))),
                (JoinKind::Left, equals(column(width), lit(0.0))),
            ] {
                assert_eq!(
                    join(kind.clone(), &approximate, &exact, pred).guarantee,
                    Some(rel(0.01)),
                    "{kind:?}"
                );
            }
            // Either side.
            assert_eq!(
                join(
                    JoinKind::Cross,
                    &exact,
                    &approximate,
                    ScalarExpr::Literal(ScalarValue::Boolean(true))
                )
                .guarantee,
                Some(rel(0.01))
            );
        }

        /// A join keyed on an approximate input's column, a semi-join, or
        /// two approximate inputs have no guarantee (column-level tracking
        /// is #689).
        #[test]
        fn join_on_an_approximate_key_has_no_guarantee() {
            let (approximate, exact, width) = approximate_and_exact();
            assert_eq!(
                join(
                    JoinKind::Inner,
                    &approximate,
                    &exact,
                    equals(column(0), column(width))
                )
                .guarantee,
                None
            );
            assert_eq!(
                join(
                    JoinKind::Semi,
                    &exact,
                    &approximate,
                    ScalarExpr::Literal(ScalarValue::Boolean(true))
                )
                .guarantee,
                None
            );
            assert_eq!(
                join(
                    JoinKind::Cross,
                    &approximate,
                    &approximate,
                    ScalarExpr::Literal(ScalarValue::Boolean(true))
                )
                .guarantee,
                None
            );
        }

        /// A `CASE` whose condition reads only exact columns keeps each
        /// branch's guarantee (a NULL branch is exact); one testing an
        /// approximate column has none.
        #[test]
        fn case_on_exact_columns_keeps_the_branch_guarantee() {
            let (approximate, exact, width) = approximate_and_exact();
            let joined = join(
                JoinKind::Cross,
                &approximate,
                &exact,
                ScalarExpr::Literal(ScalarValue::Boolean(true)),
            );
            let value = approximate.schema.fields.len() - 1;
            assert_eq!(
                case_over(&joined, equals(column(width), lit(0.0)), column(value)),
                Some(rel(0.01))
            );
            assert_eq!(
                case_over(&joined, equals(column(value), lit(0.0)), column(value)),
                None
            );
        }

        /// `node` projecting `cols` over `child`, composed.
        fn project(child: &OperatorNode, cols: Vec<ScalarExpr>) -> OperatorNode {
            let mut node = OperatorNode::new(Operator::NonASAP(NonASAPOp::Project {
                cols: cols
                    .into_iter()
                    .enumerate()
                    .map(|(i, expr)| ProjectItem {
                        alias: Some(format!("c{i}")),
                        expr,
                    })
                    .collect(),
                qualifier: None,
                child: Rc::new(child.clone()),
            }))
            .unwrap();
            node.guarantee = composed_guarantee(&node);
            node
        }

        /// Over a join, a projection reads each column's own guarantee,
        /// not the child's sample value (which may be an exact input's
        /// column): `sum(m) * 1000` beside the exact side's `value` has
        /// the scaled error 1000, not the approximate side's 1. A
        /// projection over that computed projection has no per-column
        /// guarantees, so it has none (#689).
        #[test]
        fn projection_over_a_join_reads_each_columns_guarantee() {
            let approximate = node(abs(1.0, 0.01));
            let exact = node_over("m", AccuracyGuarantee::exact("test"));
            let joined = join(
                JoinKind::Cross,
                &approximate,
                &exact,
                ScalarExpr::Literal(ScalarValue::Boolean(true)),
            );
            assert_eq!(joined.guarantee, Some(abs(1.0, 0.01)));
            let sum = approximate.schema.fields.len() - 1;
            let value = approximate.schema.fields.len() + exact.schema.fields.len() - 1;
            let scaled = project(
                &joined,
                vec![
                    arith(ArithmeticOpKind::Mul, column(sum), lit(1000.0)),
                    column(value),
                ],
            );
            assert_eq!(scaled.guarantee.as_ref().map(bound), Some(1000.0));
            let stacked = project(
                &scaled,
                vec![arith(ArithmeticOpKind::Add, column(1), lit(1.0)), column(0)],
            );
            assert_eq!(stacked.guarantee, None);
        }
    }

    /// Which stream norm bounds a Count-Min or Count Sketch estimate (#669).
    mod stream_norms {
        use super::*;
        use crate::accuracy::composition::{
            source_norm_statistic, STREAM_L1_NORM, STREAM_ROW_COUNT,
        };
        use crate::test_support::{cms_summary, weighted};

        /// A count (constant weight) is bounded by its weight times the
        /// source's row count; a sum of the source's raw value column by
        /// the source's norm; a sum of a computed value (`m * 1000`,
        /// `rate(…)`) by nothing that can be bound.
        #[test]
        fn a_norm_follows_the_update_weight() {
            let count = Rc::new(cms_summary("count by (job) (m)"));
            assert_eq!(
                stream_norm(&count),
                Some(StreamNorm::ConstantWeight {
                    weight: 1.0,
                    source: "m".into()
                })
            );
            let raw = weighted(
                cms_summary("count by (job) (m)"),
                SummaryInputExpr::Column(ColumnRef::SampleValue),
            );
            assert_eq!(stream_norm(&raw), Some(StreamNorm::Source("m".into())));
            for computed in ["count by (job) (m * 1000)", "count by (job) (rate(m[1m]))"] {
                let summary = weighted(
                    cms_summary(computed),
                    SummaryInputExpr::Column(ColumnRef::SampleValue),
                );
                assert_eq!(stream_norm(&summary), None, "{computed}");
            }
        }

        /// The absolute form of a Count-Min bound names the statistic its
        /// update weight allows: the source's norm for its raw value
        /// column, the weight times the source's row count for a count.
        #[test]
        fn the_absolute_bound_names_the_weighted_norm() {
            let frequency = AccuracyGuarantee::new(
                AccuracyMetric::ABSOLUTE_STREAM_L1,
                BoundExpr::Constant { value: 0.01 },
                ProbabilityExpr::Constant { value: 0.001 },
                vec![],
            );
            let estimate = |summary: Rc<OperatorNode>| {
                operand_guarantee(&crate::test_support::estimate_over(
                    summary,
                    frequency.clone(),
                ))
                .unwrap()
            };
            let raw = estimate(weighted(
                cms_summary("count by (job) (m)"),
                SummaryInputExpr::Column(ColumnRef::SampleValue),
            ));
            let l1 = source_norm_statistic(STREAM_L1_NORM, "m");
            assert_eq!(
                raw.bind(&|s| (s == l1).then_some(100.0))
                    .spec
                    .error
                    .evaluate(),
                Some(1.0)
            );
            let count = estimate(Rc::new(cms_summary("count by (job) (m)")));
            assert!(count
                .bind(&|s| (s == l1).then_some(100.0))
                .spec
                .error
                .has_unknown());
            let rows = source_norm_statistic(STREAM_ROW_COUNT, "m");
            assert_eq!(
                count
                    .bind(&|s| (s == rows).then_some(100.0))
                    .spec
                    .error
                    .evaluate(),
                Some(1.0)
            );
        }
    }

    fn inventory(query: &str) -> PerComputationCandidates<usize> {
        let root = crate::test_support::lower_promql(
            query,
            AccuracyTarget::max_error(0.01).failure_probability(0.001),
        );
        // Per-series state ranks each series' identity, which the rows carry
        // as a column; a query over aggregated rows is planned over labels.
        let root = asap_types::ir::schema::schema_support::with_promql_series_identity(&root)
            .unwrap_or(root);
        generate_per_computation_candidates(vec![(0, QueryRoot::Operator(root))]).unwrap()
    }

    /// Every candidate of target `t`, then of its fused computation, composed
    /// with every other target at its first candidate, with the candidate.
    fn composed_with(
        inventory: &PerComputationCandidates<usize>,
        t: usize,
    ) -> Vec<(ComputationCandidate, Rc<OperatorNode>)> {
        (0..inventory.targets[t].choice_count())
            .map(|c| {
                let mut choice = vec![0; inventory.targets.len()];
                choice[t] = c;
                let composed = compose_logical_candidate(inventory, &choice).unwrap();
                let QueryRoot::Operator(root) = &composed[0].1 else {
                    panic!("operator root")
                };
                let (candidate, _) = inventory.targets[t].computation_for(c).unwrap();
                (candidate.clone(), root.clone())
            })
            .collect()
    }

    fn heap_states(root: &Rc<OperatorNode>) -> Vec<Rc<OperatorNode>> {
        OperatorNode::reachable(root)
            .into_iter()
            .filter(|node| {
                matches!(node.asap(), Some(ASAPOp::SummaryAgg {
                    family: FieldDataType::Sketch(kind, _), ..
                }) if matches!(
                    kind.algorithm(),
                    SketchAlgorithm::CmsWithHeap | SketchAlgorithm::CountSketchWithHeap
                ))
            })
            .collect()
    }

    fn topk_target(inventory: &PerComputationCandidates<usize>) -> usize {
        inventory
            .targets
            .iter()
            .position(|t| {
                matches!(
                    t.aggregate.non_asap(),
                    Some(NonASAPOp::Aggregate { measures, .. })
                        if matches!(measures.as_slice(), [AggIntent::TopK { .. }])
                )
            })
            .expect("a top-k target")
    }

    /// Example 1's Q2: the top-k over a per-series `sum_over_time` keeps
    /// only its exact candidate and gets a fused whole-expression
    /// computation with a Count-Min and a CountSketch heap, which replace the
    /// heaps over the exact sum. The fused computation absorbs the inner sum: composed, no
    /// aggregate is computed and the heap reads the range selector's raw
    /// samples.
    #[test]
    fn whole_expression_topk_replaces_the_heap_over_the_exact_sum() {
        let inventory = inventory("topk by (job) (10, sum_over_time(m[1m]))");
        let topk = topk_target(&inventory);
        let target = &inventory.targets[topk];
        assert_eq!(target.choices.candidates, [ComputationCandidate::Exact]);
        let fused = target.fused.as_ref().expect("a fused computation");
        assert_eq!(
            fused.candidates,
            [
                ComputationCandidate::Summary(SketchAlgorithm::CmsWithHeap),
                ComputationCandidate::Summary(SketchAlgorithm::CountSketchWithHeap),
            ]
        );
        let inner = fused.absorbs[0];
        assert_eq!(inventory.targets[inner].choices.candidates.len(), 1);
        assert_eq!(inventory.combinations(), 3);
        for (candidate, root) in composed_with(&inventory, topk) {
            if candidate == ComputationCandidate::Exact {
                continue;
            }
            assert!(
                !OperatorNode::reachable(&root).iter().any(|n| matches!(
                    n.non_asap(),
                    Some(NonASAPOp::Aggregate { .. })
                ) || matches!(
                    n.asap(),
                    Some(ASAPOp::FinalizeExactAccumulator { .. })
                )),
                "{candidate:?}: the inner sum is not computed"
            );
            let [state] = &heap_states(&root)[..] else {
                panic!("one heap")
            };
            let Some(ASAPOp::SummaryAgg { child, .. }) = state.asap() else {
                unreachable!()
            };
            assert!(matches!(
                child.non_asap(),
                Some(NonASAPOp::TimeRange { .. })
            ));
        }
    }

    /// The whole-expression heap reads the inner sum's input, so the outer
    /// top-k's partition keys (indices into the sum's output) are moved onto
    /// that input by column.
    #[test]
    fn whole_expression_topk_partitions_index_the_heap_input() {
        let root = crate::test_support::lower_promql(
            "topk by (job) (2, sum by (service, job) (rate(m[1m])))",
            AccuracyTarget::max_error(0.1),
        );
        let inventory =
            generate_per_computation_candidates(vec![(0, QueryRoot::Operator(root))]).unwrap();
        let topk = topk_target(&inventory);
        let mut heaps = 0;
        for (candidate, root) in composed_with(&inventory, topk) {
            // The top-k's own candidate is exact; its summaries are the
            // fused computation's.
            if !matches!(candidate, ComputationCandidate::Summary(_)) {
                continue;
            }
            for state in heap_states(&root) {
                let Some(ASAPOp::SummaryAgg {
                    child, reduction, ..
                }) = state.asap()
                else {
                    unreachable!()
                };
                let names: Vec<_> = reduction
                    .group_keys()
                    .expect("partitioned by job")
                    .keys()
                    .iter()
                    .map(|&index| child.schema.fields[index].name.as_str())
                    .collect();
                assert_eq!(names, ["job"]);
                heaps += 1;
            }
        }
        assert_eq!(
            heaps, 2,
            "a whole-expression Count-Min and CountSketch heap"
        );
    }

    /// How many whole-workload choices enumeration keeps: every digit over
    /// its target's `choice_count`, less those [`absorbed_choice_varies`]
    /// skips.
    ///
    /// [`absorbed_choice_varies`]: PerComputationCandidates::absorbed_choice_varies
    fn enumerated_choices(inventory: &PerComputationCandidates<usize>) -> usize {
        let mut enumerated = 0;
        let mut choice = vec![0; inventory.targets.len()];
        loop {
            if !inventory.absorbed_choice_varies(&choice) {
                enumerated += 1;
            }
            let mut carry = true;
            for (digit, target) in choice.iter_mut().zip(&inventory.targets).rev() {
                *digit += 1;
                if *digit < target.choice_count() {
                    carry = false;
                    break;
                }
                *digit = 0;
            }
            if carry {
                return enumerated;
            }
        }
    }

    /// `topk(k, sum by (G) (sum_over_time(m[W])))` and its `count_over_time`
    /// form rank the groups `G` by the sum (count) of their raw samples: the
    /// top-k keeps its exact candidate and gets the fused whole-expression
    /// Count-Min and CountSketch heaps, which absorb both the grouped sum and
    /// the per-series sum (count) beneath it. Composed, no aggregate is
    /// computed: the heap reads the range selector's raw samples, keyed by
    /// `G` (less the top-k's own `by` labels, its partitions), weighted by
    /// the sample value (a sum, an open requirement on `m`) or by one (a
    /// count, proven non-negative).
    #[test]
    fn whole_expression_topk_over_a_grouped_sum_of_range_sums_reads_the_samples() {
        let samples = WeightDomain::SamplesOfMetric { metric: "m".into() };
        let unit = WeightDomain::NonNegative {
            proof: NonNegativeWeightProof::UnitCount,
        };
        let value = SummaryInputExpr::Column(ColumnRef::SampleValue);
        let one = SummaryInputExpr::Constant(1.0);
        for (query, weight, domain, partitions) in [
            (
                "topk(3, sum by (region) (sum_over_time(m[1m])))",
                &value,
                &samples,
                &[][..],
            ),
            (
                "topk(3, sum by (region) (count_over_time(m[1m])))",
                &one,
                &unit,
                &[],
            ),
            (
                "topk by (job) (3, sum by (job, region) (sum_over_time(m[1m])))",
                &value,
                &samples,
                &["job"],
            ),
        ] {
            let inventory = inventory(query);
            let topk = topk_target(&inventory);
            let target = &inventory.targets[topk];
            assert_eq!(
                target.choices.candidates,
                [ComputationCandidate::Exact],
                "{query}"
            );
            let fused = target.fused.as_ref().expect("a fused computation");
            assert_eq!(
                fused.candidates,
                [
                    ComputationCandidate::Summary(SketchAlgorithm::CmsWithHeap),
                    ComputationCandidate::Summary(SketchAlgorithm::CountSketchWithHeap),
                ],
                "{query}"
            );
            assert_eq!(fused.absorbs.len(), 2, "{query}: both aggregates beneath");
            assert_eq!(
                inventory.combinations(),
                enumerated_choices(&inventory),
                "{query}"
            );
            let mut heaps = 0;
            for (candidate, root) in composed_with(&inventory, topk) {
                if candidate == ComputationCandidate::Exact {
                    continue;
                }
                assert!(
                    !OperatorNode::reachable(&root).iter().any(|n| matches!(
                        n.non_asap(),
                        Some(NonASAPOp::Aggregate { .. })
                    ) || matches!(
                        n.asap(),
                        Some(ASAPOp::FinalizeExactAccumulator { .. })
                    )),
                    "{query} {candidate:?}: no sum is computed"
                );
                let [state] = &heap_states(&root)[..] else {
                    panic!("{query}: one heap")
                };
                let Some(ASAPOp::SummaryAgg {
                    child,
                    input,
                    reduction,
                    ..
                }) = state.asap()
                else {
                    unreachable!()
                };
                assert!(
                    matches!(child.non_asap(), Some(NonASAPOp::TimeRange { .. })),
                    "{query}: the heap reads the raw samples"
                );
                assert_eq!(
                    input.item,
                    Some(SummaryInputExpr::Column(ColumnRef::Named("region".into()))),
                    "{query}"
                );
                assert_eq!(&input.weight, weight, "{query}");
                assert_eq!(&input.weight_domain, domain, "{query}");
                let names: Vec<_> = reduction
                    .group_keys()
                    .map_or(&[][..], |keys| keys.keys())
                    .iter()
                    .map(|&index| child.schema.fields[index].name.as_str())
                    .collect();
                assert_eq!(names, partitions, "{query}");
                // The answer has the top-k's columns, `region` among them.
                assert!(
                    root.schema.fields.iter().any(|f| f.name == "region"),
                    "{query}: {:?}",
                    root.schema
                );
                heaps += 1;
            }
            assert_eq!(heaps, 2, "{query}");
        }
    }

    /// The weight domain of every heap-sketch update in every candidate of
    /// `query`'s top-k.
    fn heap_weight_domains(query: &str) -> Vec<WeightDomain> {
        let inventory = inventory(query);
        let topk = topk_target(&inventory);
        composed_with(&inventory, topk)
            .iter()
            .flat_map(|(_, root)| heap_states(root))
            .map(|state| match state.asap() {
                Some(ASAPOp::SummaryAgg { input, .. }) => input.weight_domain.clone(),
                _ => unreachable!(),
            })
            .collect()
    }

    /// Pass 1 does not read the metric's type: heap weights that are raw
    /// samples of `m` carry the open requirement that `m` is a counter, under
    /// any name (`m_total` included), for plan selection to confirm.
    #[test]
    fn heap_weights_over_raw_data_samples_record_an_open_requirement() {
        for (query, metric) in [
            ("topk by (job) (10, sum_over_time(m[1m]))", "m"),
            ("topk by (job) (10, sum_over_time(m_total[1m]))", "m_total"),
        ] {
            let domains = heap_weight_domains(query);
            assert_eq!(domains.len(), 2, "{query}");
            assert!(
                domains.iter().all(|d| *d
                    == WeightDomain::RawDataSamplesOfMetric {
                        metric: metric.into()
                    }),
                "{query}: {domains:?}"
            );
        }
    }

    /// Only raw data samples and plain sums of them: arithmetic, `rate` and other
    /// functions end the requirement.
    #[test]
    fn raw_data_samples_of_metric_ends_at_any_other_operator() {
        for (query, metric) in [
            ("sum_over_time(m[1m])", Some("m")),
            ("sum by (job) (sum_over_time(m[1m]))", Some("m")),
            ("sum_over_time(m[1m]) - 100", None),
            ("-sum_over_time(m[1m])", None),
            ("rate(m[1m])", None),
            ("max_over_time(m[1m])", None),
        ] {
            let root = crate::test_support::lower_promql(query, AccuracyTarget::exact());
            assert_eq!(
                raw_data_samples_of_metric(&root).as_deref(),
                metric,
                "{query}"
            );
        }
    }

    /// An avg's exact states are its sum then its count: any other order
    /// is refused rather than divided the wrong way round.
    #[test]
    fn avg_states_out_of_order_are_refused() {
        let mut inventory = inventory("avg by (job) (m)");
        assert_eq!(
            inventory.targets[0].choices.candidates,
            [ComputationCandidate::ExactAccumulator(vec![
                ExactKind::Sum,
                ExactKind::Count
            ])]
        );
        assert!(compose_logical_candidate(&inventory, &[0]).is_ok());
        inventory.targets[0].choices.candidates =
            vec![ComputationCandidate::ExactAccumulator(vec![
                ExactKind::Count,
                ExactKind::Sum,
            ])];
        assert!(matches!(
            compose_logical_candidate(&inventory, &[0]),
            Err(PerComputationCandidateError::Unsupported(_))
        ));
    }

    /// The target whose fused computation is a nested-sum merge.
    fn nested_sum_target(inventory: &PerComputationCandidates<usize>) -> Option<usize> {
        let merge = ComputationCandidate::ExactAccumulator(vec![ExactKind::Sum]);
        inventory.targets.iter().position(|t| {
            t.fused
                .as_ref()
                .is_some_and(|fused| fused.candidates == [merge.clone()])
        })
    }

    /// `sum by (job) (sum_over_time(m[5m]))` gets a fused nested sum
    /// beside its own accumulator: one exact Sum accumulator over the range's
    /// raw data samples, grouped by `job`, which absorbs the inner per-series sum.
    /// Composed, no per-series state is built, the value is exact and the
    /// output columns are the outer sum's.
    #[test]
    fn nested_sum_merges_into_one_exact_accumulator() {
        let inventory = inventory("sum by (job) (sum_over_time(m[5m]))");
        let outer = nested_sum_target(&inventory).expect("a nested-sum merge");
        let target = &inventory.targets[outer];
        let sum = ComputationCandidate::ExactAccumulator(vec![ExactKind::Sum]);
        assert_eq!(target.choices.candidates, std::slice::from_ref(&sum));
        let fused = target.fused.as_ref().expect("a fused computation");
        assert_eq!(fused.candidates, [sum]);
        let inner = fused.absorbs[0];
        assert_eq!(inventory.targets[inner].choices.candidates.len(), 1);
        assert_eq!(inventory.combinations(), 2);
        let composed = composed_with(&inventory, outer);
        let (_, separate) = &composed[0];
        let (_, merged) = &composed[1];
        let states: Vec<_> = OperatorNode::reachable(merged)
            .into_iter()
            .filter(|n| matches!(n.asap(), Some(ASAPOp::SummaryAgg { .. })))
            .collect();
        let [state] = states.as_slice() else {
            panic!("one accumulator: {states:?}")
        };
        let Some(ASAPOp::SummaryAgg {
            child,
            family,
            reduction,
            ..
        }) = state.asap()
        else {
            unreachable!()
        };
        assert_eq!(
            *family,
            FieldDataType::ExactAggregate(ExactKind::Sum, ExactParams::Sum)
        );
        assert!(matches!(
            child.non_asap(),
            Some(NonASAPOp::TimeRange { .. })
        ));
        let Reduction::Reduce(keys) = reduction else {
            panic!("grouped by job: {reduction:?}")
        };
        assert_eq!(
            keys.keys()
                .iter()
                .map(|&k| child.schema.fields[k].name.as_str())
                .collect::<Vec<_>>(),
            ["job"]
        );
        assert!(merged
            .guarantee
            .as_ref()
            .is_some_and(AccuracyGuarantee::is_exact));
        let names = |node: &OperatorNode| {
            node.schema
                .fields
                .iter()
                .map(|f| f.name.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(names(merged), names(separate));
        merged.validate_structure().unwrap();
    }

    /// The merge needs a sum over a sum's value: an outer sum over a rate,
    /// an outer count, or an outer sum over an inner `max` gets none.
    #[test]
    fn nested_sum_merge_needs_a_sum_of_sums() {
        for query in [
            "sum by (job) (rate(m[5m]))",
            "count by (job) (sum_over_time(m[5m]))",
            "sum by (job) (max_over_time(m[5m]))",
        ] {
            assert_eq!(nested_sum_target(&inventory(query)), None, "{query}");
        }
    }

    /// A per-series sum drops the metric name, but the raw data samples keep it:
    /// grouping the merge by `__name__` would answer the metric's name where
    /// the nested sums answer none, so no merge is offered.
    #[test]
    fn nested_sum_merge_never_groups_by_the_dropped_metric_name() {
        let query = "sum by (__name__) (sum_over_time(m[5m]))";
        assert_eq!(nested_sum_target(&inventory(query)), None);
    }

    /// Merges do not stack: in `sum(sum by (job) (sum_over_time(m[5m])))`
    /// a target with a fused computation is never absorbed, and `combinations`
    /// counts exactly the choices enumeration keeps.
    #[test]
    fn nested_sum_merges_do_not_stack() {
        let inventory = inventory("sum(sum by (job) (sum_over_time(m[5m])))");
        let absorbed: Vec<usize> = absorbed_targets(&inventory.targets).collect();
        assert_eq!(absorbed.len(), 1, "one merge");
        for &a in &absorbed {
            assert!(
                inventory.targets[a].fused.is_none(),
                "an absorbed target has a fused computation"
            );
        }
        let mut enumerated = 0;
        let mut choice = vec![0; inventory.targets.len()];
        loop {
            if !inventory.absorbed_choice_varies(&choice) {
                enumerated += 1;
            }
            let mut carry = true;
            for (digit, target) in choice.iter_mut().zip(&inventory.targets).rev() {
                *digit += 1;
                if *digit < target.choice_count() {
                    carry = false;
                    break;
                }
                *digit = 0;
            }
            if carry {
                break;
            }
        }
        assert_eq!(inventory.combinations(), enumerated);
    }

    /// `combinations` counts each choice once: picking a fused
    /// whole-expression candidate with any but the first candidate of the
    /// target it absorbs is the same candidate. An absorbed target with four
    /// candidates (an approximate inner count) gives 1 exact · 4 + 2
    /// whole-expression = 6, not 3 · 4.
    #[test]
    fn combinations_skip_choices_a_whole_expression_candidate_absorbs() {
        let inventory = inventory("topk by (job) (10, count_over_time(m[1m]))");
        let topk = topk_target(&inventory);
        let target = &inventory.targets[topk];
        let Some(fused) = &target.fused else {
            panic!("a fused computation: {:?}", target.choices.candidates)
        };
        let inner = fused.absorbs[0];
        let n = target.choices.candidates.len();
        let whole = fused.candidates.len();
        let m = inventory.targets[inner].choices.candidates.len();
        let mut enumerated = 0;
        let mut choice = vec![0; inventory.targets.len()];
        loop {
            if !inventory.absorbed_choice_varies(&choice) {
                enumerated += 1;
            }
            let mut carry = true;
            for (digit, target) in choice.iter_mut().zip(&inventory.targets).rev() {
                *digit += 1;
                if *digit < target.choice_count() {
                    carry = false;
                    break;
                }
                *digit = 0;
            }
            if carry {
                break;
            }
        }
        assert!(whole > 0 && m > 1, "{n} {whole} {m}");
        assert_eq!(inventory.combinations(), n * m + whole);
        assert_eq!(inventory.combinations(), 6);
        assert_eq!(inventory.combinations(), enumerated);
    }

    /// A quantile estimate of a metric's samples carries their sign only for
    /// φ in [0, 1]: outside it PromQL answers -Inf or +Inf. Pass 1 offers
    /// no sketch for such a φ.
    #[test]
    fn a_quantile_estimate_shares_its_raw_data_samples_sign_only_inside_the_unit_interval() {
        let estimate_of = |query: &str| {
            let inventory = inventory(query);
            (0..inventory.targets.len())
                .flat_map(|t| composed_with(&inventory, t))
                .find_map(|(_, root)| {
                    OperatorNode::reachable(&root)
                        .into_iter()
                        .find(|n| matches!(n.asap(), Some(ASAPOp::SummaryEstimate { .. })))
                })
        };
        let inside = estimate_of("quantile_over_time(0.5, m[1m])").expect("a sketch estimate");
        assert_eq!(estimates_raw_data_samples_of(&inside).as_deref(), Some("m"));
        let Some(ASAPOp::SummaryEstimate { summary_input, .. }) = inside.asap() else {
            unreachable!()
        };
        for q in [-1.0, 2.0] {
            let outside = OperatorNode::with_schema(
                Operator::ASAP(ASAPOp::SummaryEstimate {
                    summary_input: Rc::clone(summary_input),
                    query: SketchStatistic::Quantile { q },
                }),
                inside.schema.clone(),
            );
            assert_eq!(estimates_raw_data_samples_of(&outside), None, "q = {q}");
        }
        for query in [
            "quantile_over_time(-1, m[1m])",
            "quantile_over_time(2, m[1m])",
        ] {
            assert!(estimate_of(query).is_none(), "{query}: a sketch is offered");
        }
    }

    /// A quantile read from merged tumbling panes of a metric's samples is
    /// still one of the samples (or between two), so it carries their sign
    /// as the same quantile over the raw window does; φ outside [0, 1]
    /// still carries none.
    #[test]
    fn a_quantile_of_merged_panes_shares_its_raw_data_samples_sign() {
        let inventory = inventory("quantile_over_time(0.99, m[100s])");
        let estimate = (0..inventory.targets.len())
            .flat_map(|t| composed_with(&inventory, t))
            .find_map(|(_, root)| {
                OperatorNode::reachable(&root)
                    .into_iter()
                    .find(|n| matches!(n.asap(), Some(ASAPOp::SummaryEstimate { .. })))
            })
            .expect("a sketch estimate");
        let Some(ASAPOp::SummaryEstimate { summary_input, .. }) = estimate.asap() else {
            unreachable!()
        };
        let Some(ASAPOp::SummaryAgg {
            child: range,
            family,
            input,
            reduction,
            grouping,
            filter,
        }) = summary_input.asap()
        else {
            panic!("{summary_input:?}")
        };
        let chain = Rc::new(
            OperatorNode::new(Operator::ASAP(ASAPOp::PaneChain {
                child: Rc::clone(range.children()[0]),
                family: family.clone(),
                input: input.clone(),
                reduction: reduction.clone(),
                grouping: grouping.clone(),
                filter: filter.clone(),
                pane_width_ms: 10_000,
                pane_count: 10,
            }))
            .unwrap(),
        );
        let merged = Rc::new(
            OperatorNode::new(Operator::ASAP(ASAPOp::SummaryMerge {
                children: vec![chain],
            }))
            .unwrap(),
        );
        let read = |q: f64| {
            OperatorNode::with_schema(
                Operator::ASAP(ASAPOp::SummaryEstimate {
                    summary_input: Rc::clone(&merged),
                    query: SketchStatistic::Quantile { q },
                }),
                estimate.schema.clone(),
            )
        };
        assert_eq!(
            estimates_raw_data_samples_of(&read(0.99)).as_deref(),
            Some("m")
        );
        assert_eq!(
            fold_stats(&Reduction::by(vec![]), &Rc::new(read(0.99)))
                .unwrap()
                .sign_statistic,
            Some(raw_data_samples_sign_statistic("m"))
        );
        for q in [-1.0, 2.0] {
            assert_eq!(estimates_raw_data_samples_of(&read(q)), None, "q = {q}");
        }
    }

    /// The metric whose raw data samples, or plain sums of them, a composed node's
    /// values estimate: a whole-expression heap's estimates and an exact
    /// sum over the samples are; a rate or any other function is not. A
    /// fold over them names its unknown sign by that metric.
    #[test]
    fn values_estimating_raw_data_samples_name_their_sign_by_the_metric() {
        let inventory = inventory("topk by (job) (10, sum_over_time(m[1m]))");
        let topk = topk_target(&inventory);
        for (candidate, root) in composed_with(&inventory, topk) {
            let estimate = OperatorNode::reachable(&root)
                .into_iter()
                .find(|n| {
                    matches!(
                        n.asap(),
                        Some(
                            ASAPOp::SummaryEstimate { .. }
                                | ASAPOp::FinalizeExactAccumulator { .. }
                        )
                    )
                })
                .unwrap();
            assert_eq!(
                estimates_raw_data_samples_of(&estimate).as_deref(),
                Some("m"),
                "{candidate:?}"
            );
            let stats = fold_stats(&Reduction::by(vec![]), &estimate).unwrap();
            assert_eq!(
                stats.sign_statistic,
                Some(crate::accuracy::composition::raw_data_samples_sign_statistic("m"))
            );
        }
        let rate = inventory_root("sum by (job) (rate(m[1m]))");
        let finalized = OperatorNode::reachable(&rate)
            .into_iter()
            .filter(|n| matches!(n.asap(), Some(ASAPOp::FinalizeExactAccumulator { .. })))
            .collect::<Vec<_>>();
        assert!(!finalized.is_empty());
        for node in finalized {
            assert_eq!(estimates_raw_data_samples_of(&node), None);
        }
        let stats = fold_stats(&Reduction::by(vec![]), &rate).unwrap();
        assert_eq!(stats.sign_statistic, None);
    }

    /// The sketch parameters of every summary state below `root`.
    fn sketch_params(root: &Rc<OperatorNode>) -> Vec<SketchParams> {
        OperatorNode::reachable(root)
            .into_iter()
            .filter_map(|node| match node.asap() {
                Some(ASAPOp::SummaryAgg {
                    family: FieldDataType::Sketch(kind, _),
                    ..
                }) => Some(kind.params().clone()),
                _ => None,
            })
            .collect()
    }

    /// `query` composed with target `t` at the candidate `pick(t, its
    /// candidates)` returns the position of.
    fn composed_picking(
        query: &str,
        pick: impl Fn(usize, &[ComputationCandidate]) -> usize,
    ) -> Rc<OperatorNode> {
        let inventory = inventory(query);
        let choice: Vec<usize> = inventory
            .targets
            .iter()
            .enumerate()
            .map(|(t, target)| pick(t, &target.choices.candidates))
            .collect();
        let composed = compose_logical_candidate(&inventory, &choice).unwrap();
        let QueryRoot::Operator(root) = &composed[0].1 else {
            panic!("operator root")
        };
        root.clone()
    }

    fn position_of(candidates: &[ComputationCandidate], candidate: ComputationCandidate) -> usize {
        candidates
            .iter()
            .position(|c| *c == candidate)
            .expect("the candidate is offered")
    }

    fn cms(eps: f64, delta: f64) -> SketchParams {
        SketchParams::Cms {
            width: crate::accuracy::estimators::cms::cms_width(eps),
            depth: crate::accuracy::estimators::cms::cms_depth(delta),
        }
    }

    /// A combination of approximate values splits the query's budget
    /// equally among them (#669b): the difference of two Count-Min counts
    /// sizes each for ε/2 and δ/2 of the query's ε = 0.01, δ = 0.001.
    #[test]
    fn approximate_inputs_of_a_combination_share_the_budget_equally() {
        let both = composed_picking("count by (job) (a) - count by (job) (b)", |_, c| {
            position_of(c, ComputationCandidate::Summary(SketchAlgorithm::Cms))
        });
        assert!(both.guarantee.as_ref().is_some_and(|g| !g.is_exact()));
        assert_eq!(sketch_params(&both), vec![cms(0.005, 0.0005); 2]);
    }

    /// Only the inputs that are approximate in the combination share the
    /// budget: with one count exact, the other gets all of it.
    #[test]
    fn an_exact_input_takes_no_share_of_the_budget() {
        let one = composed_picking("count by (job) (a) - count by (job) (b)", |t, c| {
            if t == 0 {
                position_of(c, ComputationCandidate::Summary(SketchAlgorithm::Cms))
            } else {
                0
            }
        });
        assert_eq!(sketch_params(&one), vec![cms(0.01, 0.001)]);
    }

    /// The first composed candidate of `query`.
    fn inventory_root(query: &str) -> Rc<OperatorNode> {
        let inventory = inventory(query);
        let composed =
            compose_logical_candidate(&inventory, &vec![0; inventory.targets.len()]).unwrap();
        let QueryRoot::Operator(root) = &composed[0].1 else {
            panic!("operator root")
        };
        root.clone()
    }

    /// A Count-Min estimate has a bound only over weights proven
    /// non-negative: an open requirement (`RawDataSamplesOfMetric`) is not a proof.
    #[test]
    fn count_min_estimate_has_no_bound_over_an_open_requirement() {
        let family = FieldDataType::Sketch(
            SketchKind::new(
                SketchAlgorithm::Cms,
                asap_types::ir::schema::SketchParams::Cms { width: 8, depth: 2 },
            ),
            GroupingStrategy::default(),
        );
        let query = SketchStatistic::PointCount {
            key: ColumnRef::SampleValue,
            value: None,
        };
        let with = |weight_domain| SummaryUpdate {
            item: Some(SummaryInputExpr::Column(ColumnRef::SampleValue)),
            weight: SummaryInputExpr::Column(ColumnRef::SampleValue),
            weight_domain,
        };
        assert!(!estimate_has_bound(
            &family,
            &with(WeightDomain::RawDataSamplesOfMetric { metric: "m".into() }),
            &query
        ));
        assert!(!estimate_has_bound(
            &family,
            &with(WeightDomain::UnknownOrSigned),
            &query
        ));
        assert!(estimate_has_bound(
            &family,
            &with(WeightDomain::NonNegative {
                proof: NonNegativeWeightProof::CounterRawDataSamples
            }),
            &query
        ));
    }

    /// A Count-Min estimate over samples of `m` gets its bound gated on
    /// `m`'s sign: unknown until plan selection binds it, the local bound
    /// once bound to 1.
    #[test]
    fn count_min_estimate_over_raw_data_samples_is_gated_on_the_metric() {
        let family = FieldDataType::Sketch(
            SketchKind::new(
                SketchAlgorithm::Cms,
                asap_types::ir::schema::SketchParams::Cms {
                    width: 272,
                    depth: 5,
                },
            ),
            GroupingStrategy::default(),
        );
        let query = SketchStatistic::PointCount {
            key: ColumnRef::SampleValue,
            value: None,
        };
        let update = SummaryUpdate {
            item: Some(SummaryInputExpr::Column(ColumnRef::SampleValue)),
            weight: SummaryInputExpr::Column(ColumnRef::SampleValue),
            weight_domain: WeightDomain::RawDataSamplesOfMetric { metric: "m".into() },
        };
        let gated = estimate_bound_over_raw_data_samples(&family, &update, &query).expect("gated");
        assert_eq!(gated.spec.error.evaluate(), None);
        let local = local_guarantee(&family, &query).unwrap();
        let statistic = raw_data_samples_sign_statistic("m");
        // Every other statistic (a stream norm) bound alike.
        let others = |s: &str| Some(if s == statistic { 1.0 } else { 1_000.0 });
        let bound = gated.bind(&others).spec.error.evaluate();
        assert!(bound.is_some());
        assert_eq!(bound, local.bind(&others).spec.error.evaluate());
        let unconfirmed = gated
            .bind(&|s| (s != statistic).then_some(1_000.0))
            .spec
            .error
            .evaluate();
        assert_eq!(unconfirmed, None);
    }

    /// Classic HLL's failure probability is left symbolic in a source's
    /// distinct bound only for one instance per group over that source's
    /// own rows (#693's scope condition): not over a join of two sources,
    /// not over computed rows, and not for one Hydra structure shared by
    /// every group, whose distinct items the source's bound does not bound.
    #[test]
    fn classic_hll_confidence_needs_one_instance_over_a_sources_own_rows() {
        use asap_types::ir::schema::{HydraKind, HydraParams};
        let hll = |grouping| {
            FieldDataType::Sketch(
                SketchKind::new(
                    SketchAlgorithm::Hll,
                    SketchParams::Hll {
                        precision: 15,
                        relative_error: Some(0.05),
                    },
                ),
                grouping,
            )
        };
        let input = |query| crate::test_support::lower_promql(query, AccuracyTarget::exact());
        let guarantee = |family: &FieldDataType, query| {
            source_distinct_guarantee(family, &SketchStatistic::Cardinality, &input(query))
        };
        let own = hll(GroupingStrategy::PerSubpopulationInstance);
        let symbolic = guarantee(&own, "a").expect("a source's own rows");
        assert_eq!(
            symbolic.spec.failure_probability,
            ProbabilityExpr::Unknown {
                statistic: crate::accuracy::estimators::hll::classic_failure_statistic(
                    15, 0.05, "a"
                )
            }
        );
        for (query, over) in [("a * on() b", "a join"), ("a * 2", "computed rows")] {
            assert!(guarantee(&own, query).is_none(), "{over}: {query}");
        }
        let hydra = hll(GroupingStrategy::SharedMultiSubpopulation {
            kind: HydraKind::HydraCms,
            params: HydraParams::HydraCms {
                width: 8,
                depth: 2,
                shared_rows: 2,
                shared_columns: 8,
            },
        });
        assert!(guarantee(&hydra, "a").is_none());
    }

    /// A deployment whose executor supports only `supported`: each family
    /// with its readouts.
    fn supporting(supported: &[(SketchAlgorithm, &[StatisticKind])]) -> DeploymentCapabilities {
        DeploymentCapabilities {
            supported_summaries: Some(
                supported
                    .iter()
                    .map(|(family, readouts)| crate::SupportedSummary {
                        family: family.clone(),
                        readouts: readouts.to_vec(),
                    })
                    .collect(),
            ),
            ..DeploymentCapabilities::default()
        }
    }

    /// A family the deployment cannot build is pruned from Pass 1 with
    /// `UnsupportedByDeployment`; the supported family and the exact option
    /// stay. With every summary supported the inventory is unchanged.
    #[test]
    fn a_family_the_deployment_cannot_build_is_pruned() {
        let query = "quantile_over_time(0.99, m[5m])";
        let unrestricted = inventory(query);
        let restricted = inventory(query).without_unsupported_summaries(&supporting(&[(
            SketchAlgorithm::Kll,
            &[StatisticKind::Quantile],
        )]));
        let target = &restricted.targets[0];
        assert_eq!(
            target.choices.candidates,
            [
                ComputationCandidate::Exact,
                ComputationCandidate::Summary(SketchAlgorithm::Kll)
            ]
        );
        assert_eq!(
            target.choices.pruned,
            [(
                ComputationCandidate::Summary(SketchAlgorithm::DDSketch),
                PruneReason::UnsupportedByDeployment
            )]
        );
        let default =
            inventory(query).without_unsupported_summaries(&DeploymentCapabilities::default());
        assert_eq!(default.targets[0].choices, unrestricted.targets[0].choices);
    }

    /// A family the deployment builds but cannot answer the measure's
    /// readout from is pruned: UnivMon, supported for distinct counts and
    /// L2 only, is no count candidate; Count-Min and CountSketch, supported
    /// for the total, stay.
    #[test]
    fn a_readout_the_deployment_cannot_answer_prunes_its_family() {
        let restricted =
            inventory("count_over_time(m[1m])").without_unsupported_summaries(&supporting(&[
                (SketchAlgorithm::Cms, &[StatisticKind::Total]),
                (SketchAlgorithm::CountSketch, &[StatisticKind::Total]),
                (
                    SketchAlgorithm::UnivMon,
                    &[StatisticKind::Distinct, StatisticKind::FrequencyL2],
                ),
            ]));
        let target = &restricted.targets[0];
        assert_eq!(
            target.choices.candidates,
            [
                ComputationCandidate::ExactAccumulator(vec![ExactKind::Count]),
                ComputationCandidate::Summary(SketchAlgorithm::Cms),
                ComputationCandidate::Summary(SketchAlgorithm::CountSketch),
            ]
        );
        assert_eq!(
            target.choices.pruned,
            [(
                ComputationCandidate::Summary(SketchAlgorithm::UnivMon),
                PruneReason::UnsupportedByDeployment
            )]
        );
    }

    /// A fused whole-expression top-k loses the heap families the
    /// deployment cannot answer top-k from, each recorded with its reason,
    /// and is not offered once it has none left: the target and the inner
    /// sum keep their exact options.
    #[test]
    fn a_fused_computation_loses_its_unsupported_heaps() {
        let query = "topk by (job) (10, sum_over_time(m[1m]))";
        let one_heap = inventory(query).without_unsupported_summaries(&supporting(&[(
            SketchAlgorithm::CountSketchWithHeap,
            &[StatisticKind::TopK],
        )]));
        let topk = topk_target(&one_heap);
        assert_eq!(
            one_heap.targets[topk].fused.as_ref().unwrap().candidates,
            [ComputationCandidate::Summary(
                SketchAlgorithm::CountSketchWithHeap
            )]
        );
        assert_eq!(
            one_heap.targets[topk].fused.as_ref().unwrap().pruned,
            [(
                ComputationCandidate::Summary(SketchAlgorithm::CmsWithHeap),
                PruneReason::UnsupportedByDeployment
            )]
        );
        assert_eq!(one_heap.combinations(), 2);
        // Count-Min with a heap answers item frequencies, not top-k.
        let none = inventory(query).without_unsupported_summaries(&supporting(&[(
            SketchAlgorithm::CmsWithHeap,
            &[StatisticKind::ItemFrequency],
        )]));
        let topk = topk_target(&none);
        let fused = none.targets[topk].fused.as_ref().unwrap();
        assert!(fused.candidates.is_empty());
        assert_eq!(
            fused.pruned,
            [
                (
                    ComputationCandidate::Summary(SketchAlgorithm::CmsWithHeap),
                    PruneReason::UnsupportedByDeployment
                ),
                (
                    ComputationCandidate::Summary(SketchAlgorithm::CountSketchWithHeap),
                    PruneReason::UnsupportedByDeployment
                ),
            ]
        );
        assert_eq!(
            none.targets[topk].choices.candidates,
            [ComputationCandidate::Exact]
        );
        assert_eq!(none.combinations(), 1);
    }

    /// Mergeable intents give exactly their accumulator (`avg`: sum, count); other exact-only intents give `Exact`.
    #[test]
    fn intent_candidate_matrix() {
        use asap_types::ir::operator::agg_intent::{MathFunc, TimeFunc};
        use AggIntent as A;
        use ExactKind as E;
        let accumulator = |kinds: Vec<E>| ComputationCandidate::ExactAccumulator(kinds);
        let exact_count = A::Count {
            accuracy: AccuracyTarget::exact(),
        };
        let mut matrix: Vec<(A, ComputationChoices)> = [
            (A::Min { col: None }, vec![E::Min]),
            (A::Max { col: None }, vec![E::Max]),
            (A::IRate, vec![E::IRate]),
            (A::Increase, vec![E::Increase]),
            (A::Sum { col: None }, vec![E::Sum]),
            (A::Rate, vec![E::Rate]),
            (A::Avg { col: None }, vec![E::Sum, E::Count]),
        ]
        .into_iter()
        .map(|(intent, kinds)| {
            (
                intent,
                ComputationChoices {
                    candidates: vec![accumulator(kinds)],
                    pruned: vec![],
                },
            )
        })
        .collect();
        // An exact count prunes its summaries for requiring an exact answer.
        matrix.push((
            exact_count.clone(),
            ComputationChoices {
                candidates: vec![accumulator(vec![E::Count])],
                pruned: summary_candidates(&exact_count)
                    .iter()
                    .map(|a| {
                        (
                            ComputationCandidate::Summary(a.clone()),
                            PruneReason::RequiresExact,
                        )
                    })
                    .collect(),
            },
        ));
        let exact_only = [
            A::PearsonCorr { left: 0, right: 1 },
            A::Extension {
                ext_kind: "frequency".into(),
                payload: serde_json::json!({ "item": "checkout" }),
            },
            A::HistogramQuantile { q: 0.99, le: 0 },
            A::Changes,
            A::Delta,
            A::IDelta,
            A::Deriv,
            A::Resets,
            A::PredictLinear { seconds: 60.0 },
            A::DoubleExpSmoothing {
                smoothing: 0.5,
                trend: 0.5,
            },
            A::HistogramCount,
            A::HistogramSum,
            A::HistogramAvg,
            A::HistogramStdDev,
            A::HistogramStdVar,
            A::HistogramFraction {
                lower: 0.0,
                upper: 1.0,
            },
            A::Math(MathFunc::Abs),
            A::TimeFn(TimeFunc::Hour),
            A::Absent,
            A::AbsentOverTime,
            A::PresentOverTime,
            A::Group,
            A::CountValues { label: "v".into() },
            A::LastOverTime,
            A::FirstOverTime,
            A::MadOverTime,
            A::TsOfMinOverTime,
            A::TsOfMaxOverTime,
            A::TsOfFirstOverTime,
            A::TsOfLastOverTime,
        ];
        matrix.extend(exact_only.into_iter().map(|intent| {
            (
                intent,
                ComputationChoices {
                    candidates: vec![ComputationCandidate::Exact],
                    pruned: vec![],
                },
            )
        }));
        for (intent, expected) in &matrix {
            assert_eq!(
                &computation_candidates_for_intent(intent).unwrap(),
                expected,
                "{intent:?}"
            );
        }
        // Correlation never gets a single-input summary.
        assert!(summary_candidates(&A::PearsonCorr { left: 0, right: 1 }).is_empty());
    }

    /// `avg_over_time` divides with a finite-division guard; a grouped `avg` does not.
    #[test]
    fn temporal_average_divides_with_a_finite_division_guard() {
        let division_guard = |query: &str| {
            let root = inventory_root(query);
            let divisions: Vec<bool> = OperatorNode::reachable(&root)
                .iter()
                .filter_map(|node| match node.non_asap() {
                    Some(NonASAPOp::BinaryOp { operator, .. })
                        if operator.kind == BinaryOpKind::Arithmetic(ArithmeticOpKind::Div) =>
                    {
                        Some(operator.checked_finite_division)
                    }
                    _ => None,
                })
                .collect();
            let [guarded] = divisions[..] else {
                panic!("{query}: one sum / count division: {root:#?}")
            };
            guarded
        };
        assert!(division_guard("avg_over_time(a[5m])"));
        assert!(!division_guard("avg by (job) (m)"));
    }

    /// The candidates of `query` lowered at `accuracy`, over rows without the
    /// series-identity column (open PromQL schemas): over identity-carrying
    /// (closed) rows a grouped top-k gets no whole-expression heap.
    fn inventory_at(query: &str, accuracy: AccuracyTarget) -> PerComputationCandidates<usize> {
        let root = crate::test_support::lower_promql(query, accuracy);
        generate_per_computation_candidates(vec![(0, QueryRoot::Operator(root))]).unwrap()
    }

    fn open_inventory(query: &str) -> PerComputationCandidates<usize> {
        inventory_at(
            query,
            AccuracyTarget::max_error(0.01).failure_probability(0.001),
        )
    }

    /// The estimate and state of the top-k's fused whole-expression heap of
    /// `algorithm`.
    fn fused_heap(
        inventory: &PerComputationCandidates<usize>,
        algorithm: SketchAlgorithm,
    ) -> (SketchStatistic, Rc<OperatorNode>) {
        let topk = topk_target(inventory);
        let own = inventory.targets[topk].choices.candidates.len();
        let (_, root) = composed_with(inventory, topk)
            .into_iter()
            .skip(own)
            .find(|(candidate, _)| *candidate == ComputationCandidate::Summary(algorithm.clone()))
            .unwrap_or_else(|| panic!("a fused {algorithm:?} candidate"));
        let [state] = &heap_states(&root)[..] else {
            panic!("one heap: {root:#?}")
        };
        let estimate = OperatorNode::reachable(&root)
            .into_iter()
            .find_map(|node| match node.asap() {
                Some(ASAPOp::SummaryEstimate {
                    summary_input,
                    query,
                }) if Rc::ptr_eq(summary_input, state) => Some(query.clone()),
                _ => None,
            })
            .expect("the heap's estimate");
        (estimate, state.clone())
    }

    fn heap_update(state: &OperatorNode) -> &SummaryUpdate {
        match state.asap() {
            Some(ASAPOp::SummaryAgg { input, .. }) => input,
            _ => unreachable!(),
        }
    }

    /// A top-k over a `count` fuses into one global unit-weight heap over the raw rows, ranking the label.
    #[test]
    fn count_ranked_topk_fuses_to_one_global_heap_sketch() {
        let inventory = open_inventory("topk(10, count by (service) (m))");
        let topk = topk_target(&inventory);
        let fused = inventory.targets[topk]
            .fused
            .as_ref()
            .expect("a fused heap");
        assert!(fused
            .candidates
            .contains(&ComputationCandidate::Summary(SketchAlgorithm::CmsWithHeap)));
        let (estimate, state) = fused_heap(&inventory, SketchAlgorithm::CmsWithHeap);
        assert_eq!(estimate, SketchStatistic::TopK { k: 10 });
        let update = heap_update(&state);
        assert_eq!(
            update.item,
            Some(SummaryInputExpr::Column(ColumnRef::Named("service".into())))
        );
        assert_eq!(update.weight, SummaryInputExpr::Constant(1.0));
        assert_eq!(
            update.weight_domain,
            WeightDomain::NonNegative {
                proof: NonNegativeWeightProof::UnitCount,
            }
        );
        let Some(ASAPOp::SummaryAgg { child, .. }) = state.asap() else {
            unreachable!()
        };
        assert!(!child.contains_asap());
    }

    /// The ranked item over several group keys is their tuple, in the lowered (name-sorted) key order.
    #[test]
    fn multidimensional_ranked_item_is_a_structured_tuple() {
        let inventory = open_inventory("topk(10, count by (service, region) (m))");
        let (_, state) = fused_heap(&inventory, SketchAlgorithm::CmsWithHeap);
        let column = |name: &str| SummaryInputExpr::Column(ColumnRef::Named(name.into()));
        assert_eq!(
            heap_update(&state).item,
            Some(SummaryInputExpr::Tuple(vec![
                column("region"),
                column("service")
            ]))
        );
    }

    /// The top-k's own partition keys partition the heap and are left out of the ranked tuple.
    #[test]
    fn subpopulation_columns_are_not_part_of_the_ranked_item_tuple() {
        let inventory =
            open_inventory("topk by (region) (10, count by (service, method, region) (m))");
        let (_, state) = fused_heap(&inventory, SketchAlgorithm::CmsWithHeap);
        let column = |name: &str| SummaryInputExpr::Column(ColumnRef::Named(name.into()));
        assert_eq!(
            heap_update(&state).item,
            Some(SummaryInputExpr::Tuple(vec![
                column("method"),
                column("service")
            ]))
        );
        let Some(ASAPOp::SummaryAgg {
            child, reduction, ..
        }) = state.asap()
        else {
            unreachable!()
        };
        let keys: Vec<_> = reduction
            .group_keys()
            .expect("partitioned by region")
            .keys()
            .iter()
            .map(|&k| child.schema.fields[k].name.as_str())
            .collect();
        assert_eq!(keys, ["region"]);
    }

    /// A per-series top-k ranks series identity (column, else label set) weighted by sample value.
    #[test]
    fn temporal_per_entity_topk_uses_series_identity_and_sample_value() {
        let query = "topk by (service) (5, sum_over_time(m[1m]))";
        let with_identity = inventory(query);
        let without_identity = open_inventory(query);
        for (inventory, item) in [
            (
                with_identity,
                SummaryInputExpr::Column(ColumnRef::Named(
                    asap_types::ir::schema::PROMQL_SERIES_IDENTITY.into(),
                )),
            ),
            (
                without_identity,
                SummaryInputExpr::EntityIdentity(EntityIdentity::PromqlLabelSet {
                    excluding: vec![ColumnRef::Named("service".into())],
                }),
            ),
        ] {
            let (_, state) = fused_heap(&inventory, SketchAlgorithm::CountSketchWithHeap);
            let update = heap_update(&state);
            assert_eq!(update.item, Some(item));
            assert_eq!(
                update.weight,
                SummaryInputExpr::Column(ColumnRef::SampleValue)
            );
        }
    }

    /// A KLL quantile over an exact sum accumulator composes per node, the sum adding no error.
    #[test]
    fn nested_quantile_over_an_exact_sum_composes_per_node() {
        let sum = ComputationCandidate::ExactAccumulator(vec![ExactKind::Sum]);
        let kll = ComputationCandidate::Summary(SketchAlgorithm::Kll);
        let inventory = inventory_at(
            "quantile(0.9, sum by (job) (m))",
            AccuracyTarget::max_error(0.01).failure_probability(0.01),
        );
        let choice: Vec<usize> = inventory
            .targets
            .iter()
            .map(|target| {
                let candidates = &target.choices.candidates;
                if candidates.contains(&kll) {
                    position_of(candidates, kll.clone())
                } else {
                    position_of(candidates, sum.clone())
                }
            })
            .collect();
        let composed = compose_logical_candidate(&inventory, &choice).unwrap();
        let QueryRoot::Operator(root) = &composed[0].1 else {
            panic!("operator root")
        };
        let estimate = OperatorNode::reachable(root)
            .into_iter()
            .find(|node| matches!(node.asap(), Some(ASAPOp::SummaryEstimate { .. })))
            .expect("a quantile estimate");
        let Some(ASAPOp::SummaryEstimate { summary_input, .. }) = estimate.asap() else {
            unreachable!()
        };
        let Some(ASAPOp::SummaryAgg { child, family, .. }) = summary_input.asap() else {
            panic!("a KLL state: {summary_input:#?}")
        };
        let FieldDataType::Sketch(kind, _) = family else {
            panic!("a sketch: {family:?}")
        };
        assert_eq!(kind.algorithm(), &SketchAlgorithm::Kll);
        assert_eq!(
            kind.params(),
            &SketchParams::Kll {
                k: 269,
                failure_probability: None
            }
        );
        let Some(ASAPOp::FinalizeExactAccumulator { child: sum_state }) = child.asap() else {
            panic!("the finalized sum: {child:#?}")
        };
        let Some(ASAPOp::SummaryAgg {
            family: sum_family,
            child: leaf,
            ..
        }) = sum_state.asap()
        else {
            panic!("the sum's accumulator: {sum_state:#?}")
        };
        assert_eq!(
            sum_family,
            &FieldDataType::ExactAggregate(ExactKind::Sum, ExactParams::Sum)
        );
        assert!(!leaf.contains_asap());
        assert!(child
            .guarantee
            .as_ref()
            .is_some_and(AccuracyGuarantee::is_exact));
        let guarantee = root.guarantee.as_ref().expect("a rank bound");
        assert_eq!(guarantee.spec.accuracy_metric, AccuracyMetric::Rank);
        assert_eq!(
            guarantee.spec.error.evaluate(),
            Some(crate::accuracy::estimators::kll::kll_rank_error_99(269))
        );
    }
}
