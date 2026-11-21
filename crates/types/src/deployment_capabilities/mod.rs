//! What the deployment's executor can run and keep (the third
//! deployment input, beside the cost and accuracy models).
//!
//! [`DeploymentCapabilities`] says six things:
//!
//! * which sketch summary types the executor supports, and which readouts
//!   it supports for each. A summary type is supported when the executor can build
//!   its state, and a readout of it when the executor can answer that
//!   statistic ([`StatisticKind`](crate::summary_type::StatisticKind)) from
//!   the state. Stage 2 prunes every build of an unsupported summary type
//!   and every readout the executor does not answer; what reads only them
//!   loses its producer. Exact accumulators and plain exact execution are
//!   always supported;
//! * whether raw data samples are retained. If they are, Stage 3 counts them on
//!   disk once per source, at the longest window a query-time scan of it
//!   reads. If they are not, raw data samples are not retained after they arrive, so
//!   Stage 2 prunes every scan of raw data at query time or kept across
//!   evaluations: only ingestion-time work reads raw
//!   rows. Stored data (at rest or mixed) with no raw data retained is a
//!   contradiction, refused as bad input ([`DeploymentCapabilities::validate`]);
//! * whether query-time results are retained across evaluations. If they
//!   are, Stage 2 also offers a tumbling-window chain whose queries evaluate
//!   together at its tumbling-window width as query-time `AcrossEvaluations`: each
//!   evaluation builds one new tumbling window and keeps the others for the next ones,
//!   covering the queries' longest look-back. A chain two or more queries
//!   read keeps its `OneEvaluation` option too (shared within one evaluation),
//!   and Stage 3 picks the cheaper. If they are not, a query-time result is
//!   reused only within one evaluation;
//! * what the executor's counter functions do (`counter_semantics`):
//!   whether they handle counter resets and extrapolate to the range's ends.
//!   One it lacks rules out the counter functions and counter states that
//!   need it;
//! * which state families it cannot merge across windows
//!   (`state_not_mergeable_across_windows`): it builds and finalizes their
//!   state within one evaluation only, so their tumbling-window chains and
//!   builds kept across evaluations are ruled out;
//! * further candidates the caller rules out directly
//!   (`excluded_candidates`), e.g. a benchmark arm that must not scan raw
//!   data at query time.
//!
//! Every capability a deployment lacks expands into [`CandidateExclusion`]s
//! ([`DeploymentCapabilities::exclusions`]), each with the capability it
//! comes from ([`ExclusionSource`]). The planner facade expands them once
//! and hands the list to Pass 1 (which skips the summaries they rule out
//! outright) and Stage 2, which applies them as one filter:
//! a physical candidate one of them matches is pruned with
//! `PruneReason::ExcludedByDeployment`, and its consumers lose it as an
//! input.
//!
//! The default supports everything the planner can build, retains raw data
//! and keeps no query-time result across evaluations, which is the
//! planner's behavior without deployment inputs: its only exclusion is the
//! one for query-time results kept across evaluations.

// The executor lists the summary types it builds as `SupportedSummary`s, so the
// type lives with the readout vocabulary in `asap-types`.
use crate::ir::operator::AggIntent;
use crate::ir::operator::OperatorKind;
use crate::ir::properties::ExecutionTiming;
use crate::ir::schema::FieldDataType;
pub use crate::summary_type::SupportedSummary;
use crate::summary_type::{ExactKind, SketchAlgorithm, SummaryFamily, SummaryTypeCapability};
use crate::workload::{DataArrival, DataWorkload};
use serde::{Deserialize, Serialize};
use std::fmt;
use thiserror::Error;

mod exclusion;
#[cfg(test)]
mod tests;

pub use exclusion::{CandidateDescription, CandidateExclusion, SketchReadout};

/// What the deployment's executor can run and keep. Planning reads it in
/// Pass 1 and Stage 2, as the exclusions it expands into
/// ([`DeploymentCapabilities::exclusions`]). It is data only; `asap_planner` reads it from
/// `--deployment-capabilities <json>` and shows it in the stage document.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DeploymentCapabilities {
    /// The sketch summary types the executor can build, each with the
    /// readouts it can answer from one; `None` (the default) supports every summary type
    /// and readout the planner can build.
    pub supported_summaries: Option<Vec<SupportedSummary>>,
    /// Whether raw data samples are retained for query-time scans; default `true`.
    pub raw_data_retained: bool,
    /// Whether a query-time result may be materialized across evaluations
    /// (query-time `AcrossEvaluations`); default `false`.
    pub query_time_results_retained: bool,
    /// What the executor's counter functions do; default: they handle
    /// counter resets and extrapolate, as PromQL's do.
    pub counter_semantics: CounterSemanticsSupport,
    /// The state families the executor cannot merge across windows: it
    /// builds and finalizes their state within one evaluation only, so no
    /// tumbling-window chain or state kept across evaluations of them is
    /// offered. E.g. a target without mergeable counter state lists
    /// `{"exact": "Counter"}`. Default: none.
    pub state_not_mergeable_across_windows: Vec<SummaryFamily>,
    /// Candidates the caller rules out directly, beyond what the fields
    /// above imply; default none. E.g. `[{"timing": "query_time",
    /// "operator": "Scan"}]` leaves only plans that read stored state.
    pub excluded_candidates: Vec<CandidateExclusion>,
}

impl Default for DeploymentCapabilities {
    fn default() -> Self {
        Self {
            supported_summaries: None,
            raw_data_retained: true,
            query_time_results_retained: false,
            counter_semantics: CounterSemanticsSupport::default(),
            state_not_mergeable_across_windows: Vec::new(),
            excluded_candidates: Vec::new(),
        }
    }
}

/// What a deployment's counter functions (`rate`, `increase`, `irate`,
/// `resets`, and `delta`'s extrapolation) do. JSON: an object; an omitted
/// field is `true`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CounterSemanticsSupport {
    /// A counter that drops to a lower value is taken to have reset, and
    /// the drop is added back (`rate`, `increase`, `irate`, `resets`).
    pub reset_handling: bool,
    /// The increase over the samples is extrapolated to the range's ends
    /// (`rate`, `increase`, `delta`).
    pub extrapolation: bool,
}

impl Default for CounterSemanticsSupport {
    fn default() -> Self {
        Self {
            reset_handling: true,
            extrapolation: true,
        }
    }
}

/// Which capability an exclusion comes from: the reason Stage 2 records
/// for a candidate it matches (`PruneReason::ExcludedByDeployment`).
/// JSON: the snake_case variant name, e.g. `"raw_data_not_retained"`, or
/// `{"excluded_candidate": {"index": 0}}`. Printed (`Display`) as the
/// capabilities' JSON names it: `raw_data_not_retained`, or
/// `excluded_candidates[0]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExclusionSource {
    /// `supported_summaries` does not list the sketch algorithm.
    SummaryNotSupported,
    /// `supported_summaries` lists the sketch algorithm, but not this
    /// readout of it.
    ReadoutNotSupported,
    /// `raw_data_retained` is false: no query-time scan of raw data, and no
    /// raw rows kept across evaluations.
    RawDataNotRetained,
    /// `query_time_results_retained` is false: no query-time result kept
    /// across evaluations.
    QueryTimeResultsNotRetained,
    /// `counter_semantics.reset_handling` is false.
    CounterResetsNotHandled,
    /// `counter_semantics.extrapolation` is false.
    CounterNotExtrapolated,
    /// `state_not_mergeable_across_windows` lists the state's family.
    StateNotMergeableAcrossWindows,
    /// The caller's `excluded_candidates[index]`.
    ExcludedCandidate { index: usize },
}

impl ExclusionSource {
    /// Whether the default deployment ([`DeploymentCapabilities::default`])
    /// has this source too: only `QueryTimeResultsNotRetained`, which is
    /// planning as it is without deployment inputs. A rejection blames the
    /// deployment only for the other sources.
    pub fn in_default_deployment(self) -> bool {
        matches!(self, Self::QueryTimeResultsNotRetained)
    }
}

impl fmt::Display for ExclusionSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::SummaryNotSupported => "summary_not_supported",
            Self::ReadoutNotSupported => "readout_not_supported",
            Self::RawDataNotRetained => "raw_data_not_retained",
            Self::QueryTimeResultsNotRetained => "query_time_results_not_retained",
            Self::CounterResetsNotHandled => "counter_resets_not_handled",
            Self::CounterNotExtrapolated => "counter_not_extrapolated",
            Self::StateNotMergeableAcrossWindows => "state_not_mergeable_across_windows",
            Self::ExcludedCandidate { index } => {
                return write!(f, "excluded_candidates[{index}]");
            }
        };
        f.write_str(name)
    }
}

/// One exclusion a deployment's capabilities expand into, with the
/// capability it comes from.
#[derive(Debug, Clone, PartialEq)]
pub struct DeploymentExclusion {
    pub source: ExclusionSource,
    pub exclusion: CandidateExclusion,
}

impl DeploymentCapabilities {
    /// The exclusions these capabilities expand into, the caller's
    /// `excluded_candidates` last (in their order): the one list Pass 1 and
    /// Stage 2 read. A capability the deployment has expands into nothing,
    /// so the default's only exclusion is `{timing: query_time,
    /// kept_across_evaluations: true}` from `query_time_results_retained:
    /// false`. E.g. `raw_data_retained: false` expands into `{timing:
    /// query_time, operator: Scan}` and `{operator: Scan,
    /// kept_across_evaluations: true}`, and `counter_semantics.extrapolation:
    /// false` into one exclusion each for the measures `rate`, `increase` and
    /// `delta` and the `Rate` and `Increase` states. A new capability is a
    /// field, its expansion here and its [`ExclusionSource`].
    pub fn exclusions(&self) -> Vec<DeploymentExclusion> {
        let mut exclusions = Vec::new();
        let mut exclude = |source, exclusion| {
            exclusions.push(DeploymentExclusion { source, exclusion });
        };
        if let Some(supported) = &self.supported_summaries {
            // A sketch algorithm not listed: none of its states is built.
            for &algorithm in SketchAlgorithm::ALL {
                if !supported.iter().any(|s| s.summary_type == algorithm) {
                    exclude(
                        ExclusionSource::SummaryNotSupported,
                        CandidateExclusion {
                            summary_family: Some(SummaryFamily::Sketch(algorithm)),
                            ..Default::default()
                        },
                    );
                }
            }
            // A listed algorithm: each statistic its state answers (its
            // capability table row) that the executor does not read from it.
            for listed in supported {
                let answered =
                    SummaryTypeCapability::of(&FieldDataType::Sketch(listed.summary_type)).readouts;
                for &(statistic, _) in answered {
                    if !listed.readouts.contains(&statistic) {
                        exclude(
                            ExclusionSource::ReadoutNotSupported,
                            CandidateExclusion {
                                readout: Some(SketchReadout {
                                    summary_type: listed.summary_type,
                                    statistic,
                                }),
                                ..Default::default()
                            },
                        );
                    }
                }
            }
        }
        if !self.raw_data_retained {
            exclude(
                ExclusionSource::RawDataNotRetained,
                CandidateExclusion {
                    timing: Some(ExecutionTiming::QueryTime),
                    operator: Some(OperatorKind::Scan),
                    ..Default::default()
                },
            );
            exclude(
                ExclusionSource::RawDataNotRetained,
                CandidateExclusion {
                    operator: Some(OperatorKind::Scan),
                    kept_across_evaluations: Some(true),
                    ..Default::default()
                },
            );
        }
        if !self.query_time_results_retained {
            exclude(
                ExclusionSource::QueryTimeResultsNotRetained,
                CandidateExclusion {
                    timing: Some(ExecutionTiming::QueryTime),
                    kept_across_evaluations: Some(true),
                    ..Default::default()
                },
            );
        }
        // A counter semantics left out: the counter functions (`Aggregate`
        // measures) and counter states that need it.
        let counters = [
            (
                self.counter_semantics.reset_handling,
                ExclusionSource::CounterResetsNotHandled,
                [
                    AggIntent::Rate,
                    AggIntent::Increase,
                    AggIntent::IRate,
                    AggIntent::Resets,
                ]
                .as_slice(),
                [ExactKind::Counter, ExactKind::IRate].as_slice(),
            ),
            (
                self.counter_semantics.extrapolation,
                ExclusionSource::CounterNotExtrapolated,
                [AggIntent::Rate, AggIntent::Increase, AggIntent::Delta].as_slice(),
                [ExactKind::Counter].as_slice(),
            ),
        ];
        for (supported, source, functions, states) in counters {
            if supported {
                continue;
            }
            for function in functions {
                exclude(
                    source,
                    CandidateExclusion {
                        aggregate_measure: Some(function.kind_name()),
                        ..Default::default()
                    },
                );
            }
            for &kind in states {
                exclude(
                    source,
                    CandidateExclusion {
                        summary_family: Some(SummaryFamily::Exact(kind)),
                        ..Default::default()
                    },
                );
            }
        }
        // A family not mergeable across windows: its tumbling-window chains
        // (a `SummaryMerge` combines their windows) and its builds kept
        // across evaluations (each evaluation merges in the rows that
        // arrived since).
        for &family in &self.state_not_mergeable_across_windows {
            exclude(
                ExclusionSource::StateNotMergeableAcrossWindows,
                CandidateExclusion {
                    summary_family: Some(family),
                    operator: Some(OperatorKind::SummaryTumblingWindowChain),
                    ..Default::default()
                },
            );
            exclude(
                ExclusionSource::StateNotMergeableAcrossWindows,
                CandidateExclusion {
                    summary_family: Some(family),
                    kept_across_evaluations: Some(true),
                    ..Default::default()
                },
            );
        }
        for (index, exclusion) in self.excluded_candidates.iter().enumerate() {
            exclude(
                ExclusionSource::ExcludedCandidate { index },
                exclusion.clone(),
            );
        }
        exclusions
    }

    /// Checks these capabilities against the data workload: stored data
    /// (at rest, or mixed) is raw data the deployment keeps, so it
    /// contradicts `raw_data_retained: false`. Plan selection refuses such
    /// inputs before measuring any candidate. An exclusion of
    /// `excluded_candidates` that sets no field (it would exclude every
    /// candidate), two of whose fields match no operator in common (a
    /// `readout` with an `operator: SummaryAgg`), whose range is empty or
    /// names an α outside (0, 1), whose `summary_family` its
    /// `sketch_parameter_range` contradicts, or whose `aggregate_measure` names no
    /// aggregate is bad input too
    /// ([`DeploymentCapabilitiesError::InvalidExclusion`]).
    pub fn validate(&self, data: &DataWorkload) -> Result<(), DeploymentCapabilitiesError> {
        if let DataArrival::AtRest | DataArrival::Mixed { .. } = &data.arrival {
            if !self.raw_data_retained {
                return Err(DeploymentCapabilitiesError::StoredDataNotRetained {
                    arrival: data.arrival,
                });
            }
        }
        for (index, exclusion) in self.excluded_candidates.iter().enumerate() {
            if let Some(reason) = exclusion.invalid_reason() {
                return Err(DeploymentCapabilitiesError::InvalidExclusion { index, reason });
            }
        }
        Ok(())
    }
}

/// Deployment capabilities that contradict the other planning inputs
/// ([`DeploymentCapabilities::validate`]); always bad input.
#[derive(Debug, Clone, PartialEq, Error)]
pub enum DeploymentCapabilitiesError {
    /// Bad input: the data workload declares stored raw data (`AtRest` or
    /// `Mixed`), but the deployment says it retains none.
    #[error("the data is stored ({arrival:?}), but the deployment does not retain raw data")]
    StoredDataNotRetained {
        /// The declared arrival that stores raw data.
        arrival: DataArrival,
    },
    /// Bad input: `excluded_candidates[index]` cannot be applied.
    #[error("excluded candidate {index}: {reason}")]
    InvalidExclusion {
        /// Its index in `excluded_candidates`.
        index: usize,
        /// Why, e.g. "it sets no field, so it would exclude every candidate".
        reason: String,
    },
}
