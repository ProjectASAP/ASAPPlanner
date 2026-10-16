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
//!   a candidate that scans raw data at query time is invalid: only
//!   ingestion-time work reads raw rows;
//! * whether a query-time result (`MatAtQuery`) may be kept across
//!   evaluations. Its retention is derived from its consumers: a
//!   tumbling-pane chain whose every reader repeats at the pane width
//!   builds one new pane per evaluation and keeps the others for the next
//!   ones, covering the readers' longest look-back. Without it, a
//!   `MatAtQuery` result is reused only within one evaluation.
//!
//! The default supports everything the planner can build, retains raw data
//! and keeps no query-time result across evaluations, which is the
//! planner's behavior without deployment inputs.

use asap_types::post_asap::{SketchAlgorithm, StatisticKind};
use serde::{Deserialize, Serialize};

/// What the deployment's executor can run and keep. Planning reads it in
/// Stage 1 (`supported_summaries`), Stage 2 and Stage 3 (the two
/// retention switches). It is data only; `stage_pipeline` reads it from
/// `--deployment <json>` and shows it in the stage document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DeploymentCapabilities {
    /// The sketch families the executor can build, each with the readouts
    /// it can answer from one; `None` (the default) supports every family
    /// and readout the planner can build.
    pub supported_summaries: Option<Vec<SupportedSummary>>,
    /// Whether raw samples are kept for query-time scans; default `true`.
    pub raw_data_retained: bool,
    /// Whether a query-time (`MatAtQuery`) result may be kept across
    /// evaluations; default `false`.
    pub query_time_retention: bool,
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
            query_time_retention: false,
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
        assert!(default.raw_data_retained && !default.query_time_retention);
        let parsed: DeploymentCapabilities = serde_json::from_str(
            r#"{"supported_summaries": [{"family": "Kll", "readouts": ["Quantile"]}]}"#,
        )
        .unwrap();
        assert!(parsed.supports(&SketchAlgorithm::Kll, StatisticKind::Quantile));
        assert!(!parsed.supports(&SketchAlgorithm::Kll, StatisticKind::Total));
        assert!(!parsed.supports(&SketchAlgorithm::DDSketch, StatisticKind::Quantile));
        assert!(parsed.raw_data_retained && !parsed.query_time_retention);
        assert!(serde_json::from_str::<DeploymentCapabilities>(r#"{"raw_data": false}"#).is_err());
    }
}
