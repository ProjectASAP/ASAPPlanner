//! What the deployment's executor can run and keep (#509's third
//! deployment input, beside the cost and accuracy models).
//!
//! [`DeploymentCapabilities`] says three things:
//!
//! * which sketch families the executor supports, and which readouts it
//!   supports for each. A family is supported when the executor can build
//!   its state, and a readout of it when the executor can answer that
//!   statistic ([`StatisticKind`]) from the state. Stage 1 (Pass 1) offers
//!   no summary candidate whose family or readout is unsupported, so no
//!   Pass 2 candidate has one either (Pass 2 rules share, split into panes
//!   or reuse existing builds; none adds a family or a readout). Exact
//!   accumulators and plain exact execution are always supported;
//! * whether raw samples are retained. If they are, Stage 3 counts them on
//!   disk once per source, at the longest window a query-time scan of it
//!   reads. If they are not, raw samples are not kept after they arrive, so
//!   a candidate that scans raw data at query time is invalid (the
//!   rejection lists every such scan): only ingestion-time work reads raw
//!   rows. Stored data (at rest or mixed) with no raw data retained is a
//!   contradiction, refused as bad input ([`DeploymentCapabilities::validate`]);
//! * whether query-time results are retained across evaluations. If they
//!   are, Stage 2 also offers a tumbling-pane chain whose readers evaluate
//!   together at its pane width as `MatAtQueryKeptAcrossEvaluations`: each
//!   evaluation builds one new pane and keeps the others for the next ones,
//!   covering the readers' longest look-back. A chain two or more queries
//!   read keeps its `MatAtQuery` option too (shared within one evaluation),
//!   and Stage 3 picks the cheaper. If they are not, a query-time result is
//!   reused only within one evaluation.
//!
//! The default supports everything the planner can build, retains raw data
//! and keeps no query-time result across evaluations, which is the
//! planner's behavior without deployment inputs.

use asap_types::ir::schema::{SketchAlgorithm, StatisticKind};
use asap_types::workload::{DataArrival, DataWorkload};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// What the deployment's executor can run and keep. Planning reads it in
/// Stage 1 (`supported_summaries`), Stage 2 (`query_time_results_retained`)
/// and Stage 3 (`raw_data_retained`). It is data only; `stage_pipeline` reads it from
/// `--deployment-capabilities <json>` and shows it in the stage document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DeploymentCapabilities {
    /// The sketch families the executor can build, each with the readouts
    /// it can answer from one; `None` (the default) supports every family
    /// and readout the planner can build.
    pub supported_summaries: Option<Vec<SupportedSummary>>,
    /// Whether raw samples are kept for query-time scans; default `true`.
    pub raw_data_retained: bool,
    /// Whether a query-time result may be kept across evaluations
    /// (`MatAtQueryKeptAcrossEvaluations`); default `false`.
    pub query_time_results_retained: bool,
}

/// One sketch family the executor can build, with the readouts it can
/// answer from the family's state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SupportedSummary {
    /// The family the executor can build.
    pub family: SketchAlgorithm,
    /// The statistics it can answer from one state of the family.
    pub readouts: Vec<StatisticKind>,
}

impl Default for DeploymentCapabilities {
    fn default() -> Self {
        Self {
            supported_summaries: None,
            raw_data_retained: true,
            query_time_results_retained: false,
        }
    }
}

impl DeploymentCapabilities {
    /// Whether the executor can build `family` and answer `readout` from it.
    pub fn supports(&self, family: &SketchAlgorithm, readout: StatisticKind) -> bool {
        self.supported_summaries.as_ref().is_none_or(|supported| {
            supported
                .iter()
                .any(|s| s.family == *family && s.readouts.contains(&readout))
        })
    }

    /// Checks these capabilities against the data workload: stored data
    /// (at rest, or mixed) is raw data the deployment keeps, so it
    /// contradicts `raw_data_retained: false`. Plan selection refuses such
    /// inputs before measuring any candidate.
    pub fn validate(&self, data: &DataWorkload) -> Result<(), DeploymentCapabilitiesError> {
        match &data.arrival {
            DataArrival::AtRest | DataArrival::Mixed { .. } if !self.raw_data_retained => {
                Err(DeploymentCapabilitiesError::StoredDataNotRetained {
                    arrival: data.arrival,
                })
            }
            _ => Ok(()),
        }
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
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The default supports every family and readout, retains raw data and
    /// keeps no query-time result across evaluations; a JSON object with
    /// only some fields keeps the defaults for the rest.
    #[test]
    fn default_supports_everything_and_json_fills_the_rest() {
        let default = DeploymentCapabilities::default();
        assert!(default.supports(&SketchAlgorithm::Theta, StatisticKind::TopK));
        assert!(default.raw_data_retained && !default.query_time_results_retained);
        let parsed: DeploymentCapabilities = serde_json::from_str(
            r#"{"supported_summaries": [{"family": "Kll", "readouts": ["Quantile"]}]}"#,
        )
        .unwrap();
        assert!(parsed.supports(&SketchAlgorithm::Kll, StatisticKind::Quantile));
        assert!(!parsed.supports(&SketchAlgorithm::Kll, StatisticKind::Total));
        assert!(!parsed.supports(&SketchAlgorithm::DDSketch, StatisticKind::Quantile));
        assert!(parsed.raw_data_retained && !parsed.query_time_results_retained);
        assert!(serde_json::from_str::<DeploymentCapabilities>(r#"{"raw_data": false}"#).is_err());
    }
}
