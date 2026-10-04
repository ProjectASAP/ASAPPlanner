//! Deployment capabilities (#509 "Deployment inputs", #525): what the
//! deployment executing a plan can build, read out and keep. With the cost
//! and accuracy models, these are the deployment's inputs to planning; plan
//! selection rejects a candidate that needs a capability listed as missing.
//!
//! This is data only. The planner names no deployment: a deployment (for
//! example the reference executor) exports its own set, and the default is
//! unrestricted.
use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::ir::schema::{
    ExactKind, FieldDataType, GroupingStrategy, HydraKind, SketchAlgorithm, SketchStatistic,
};

/// What a deployment can do. [`DeploymentCapabilities::UNRESTRICTED`] is the
/// default.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeploymentCapabilities {
    /// The summaries the deployment can build, each with the readouts it can
    /// compute from them; `None`: every summary and readout.
    pub summaries: Option<Vec<SummarySupport>>,
    /// Can maintain state at ingestion time, as data arrives.
    pub ingestion_time: bool,
    /// Can keep query-time results across evaluations (#509 Example 4, B3).
    /// No candidate needs it until Stage 2 offers that option.
    pub query_time_retention: bool,
    /// Most bytes of state a plan may retain across evaluations.
    pub memory_budget_bytes: Option<u64>,
    /// The deployment keeps the raw data of the workload's sources anyway,
    /// so a plan reading raw data at query time adds no retention (Q48).
    pub raw_data_retained: bool,
    /// Bytes one raw sample takes in retention, to price raw data a plan
    /// makes the deployment keep when `raw_data_retained` is false.
    pub raw_bytes_per_sample: u64,
}

impl DeploymentCapabilities {
    /// Every summary and readout, both timings, no memory budget, and raw
    /// data kept anyway (so no raw retention is priced). One raw sample is
    /// 16 bytes: an 8-byte timestamp and an 8-byte value, uncompressed.
    pub const UNRESTRICTED: Self = Self {
        summaries: None,
        ingestion_time: true,
        query_time_retention: true,
        memory_budget_bytes: None,
        raw_data_retained: true,
        raw_bytes_per_sample: 16,
    };

    /// Why the deployment cannot build `family`, if it cannot.
    pub fn missing_summary(&self, family: &FieldDataType) -> Option<String> {
        let Some(summaries) = &self.summaries else {
            return None;
        };
        let (summary, layout) = summary_of(family)?;
        (!summaries
            .iter()
            .any(|s| s.family == summary && s.layout == layout))
        .then(|| format!("deployment lacks a {} summary", name(&summary, &layout)))
    }

    /// Why the deployment cannot read `statistic` out of `family`, if it
    /// cannot.
    pub fn missing_readout(
        &self,
        family: &FieldDataType,
        statistic: &SketchStatistic,
    ) -> Option<String> {
        let Some(summaries) = &self.summaries else {
            return None;
        };
        let (summary, layout) = summary_of(family)?;
        let readout = Readout::of(statistic);
        (!summaries
            .iter()
            .any(|s| s.family == summary && s.layout == layout && s.readouts.contains(&readout)))
        .then(|| {
            format!(
                "deployment lacks a {} {readout:?} readout",
                name(&summary, &layout)
            )
        })
    }
}

impl Default for DeploymentCapabilities {
    fn default() -> Self {
        Self::UNRESTRICTED
    }
}

/// One summary the deployment can build, and what it can read out of it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SummarySupport {
    pub family: SummaryFamily,
    pub layout: InstanceLayout,
    /// Sketch readouts. An exact state is finalized, not read out, so an
    /// exact family lists none.
    pub readouts: BTreeSet<Readout>,
}

/// A summary family, without its sizing parameters.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum SummaryFamily {
    Exact(ExactKind),
    Sketch(SketchAlgorithm),
}

/// How a summary's instances are laid out over the groups of a reduction.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum InstanceLayout {
    /// One instance per group.
    PerGroup,
    /// One shared Hydra structure for every group.
    Hydra(HydraKind),
}

/// A statistic read out of a sketch, without its arguments.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Readout {
    Quantile,
    /// The total count of a state (a point count without an item).
    TotalCount,
    /// The count of one item.
    ItemCount,
    Cardinality,
    TopK,
    FrequencyL2,
    FrequencyEntropy,
}

impl Readout {
    pub fn of(statistic: &SketchStatistic) -> Self {
        match statistic {
            SketchStatistic::Quantile { .. } => Self::Quantile,
            SketchStatistic::PointCount { value: None, .. } => Self::TotalCount,
            SketchStatistic::PointCount { value: Some(_), .. } => Self::ItemCount,
            SketchStatistic::Cardinality => Self::Cardinality,
            SketchStatistic::TopK { .. } => Self::TopK,
            SketchStatistic::FrequencyL2 => Self::FrequencyL2,
            SketchStatistic::FrequencyEntropy => Self::FrequencyEntropy,
        }
    }
}

/// The family and layout of a summary state; `None` for other values.
pub fn summary_of(family: &FieldDataType) -> Option<(SummaryFamily, InstanceLayout)> {
    match family {
        FieldDataType::ExactAggregate(kind, _) => {
            Some((SummaryFamily::Exact(kind.clone()), InstanceLayout::PerGroup))
        }
        FieldDataType::Sketch(kind, grouping) => Some((
            SummaryFamily::Sketch(kind.algorithm().clone()),
            match grouping {
                GroupingStrategy::PerSubpopulationInstance => InstanceLayout::PerGroup,
                GroupingStrategy::SharedMultiSubpopulation { kind, .. } => {
                    InstanceLayout::Hydra(kind.clone())
                }
            },
        )),
        _ => None,
    }
}

/// E.g. "Kll", "HydraCms" or "exact Sum".
fn name(family: &SummaryFamily, layout: &InstanceLayout) -> String {
    match (family, layout) {
        (_, InstanceLayout::Hydra(kind)) => format!("{kind:?}"),
        (SummaryFamily::Sketch(algorithm), _) => format!("{algorithm:?}"),
        (SummaryFamily::Exact(kind), _) => format!("exact {kind:?}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::schema::{HydraParams, SketchKind, SketchParams};

    fn hydra_cms() -> FieldDataType {
        let (width, depth) = (64, 4);
        FieldDataType::Sketch(
            SketchKind::new(SketchAlgorithm::Cms, SketchParams::Cms { width, depth }),
            GroupingStrategy::SharedMultiSubpopulation {
                kind: HydraKind::HydraCms,
                params: HydraParams::HydraCms {
                    width,
                    depth,
                    shared_rows: 4,
                    shared_columns: 256,
                },
            },
        )
    }

    /// Unrestricted capabilities accept every summary and readout.
    #[test]
    fn unrestricted_accepts_everything() {
        let caps = DeploymentCapabilities::default();
        assert_eq!(caps, DeploymentCapabilities::UNRESTRICTED);
        assert_eq!(caps.missing_summary(&hydra_cms()), None);
        assert_eq!(
            caps.missing_readout(&hydra_cms(), &SketchStatistic::TopK { k: 1 }),
            None
        );
    }

    /// A listed summary accepts only its listed readouts; an unlisted one,
    /// or another layout of a listed family, is missing, named precisely.
    #[test]
    fn listed_summaries_and_readouts_bound_what_is_accepted() {
        let caps = DeploymentCapabilities {
            summaries: Some(vec![SummarySupport {
                family: SummaryFamily::Sketch(SketchAlgorithm::Cms),
                layout: InstanceLayout::Hydra(HydraKind::HydraCms),
                readouts: BTreeSet::from([Readout::ItemCount]),
            }]),
            ..DeploymentCapabilities::UNRESTRICTED
        };
        let item = SketchStatistic::PointCount {
            key: crate::ir::scalar::ColumnRef::Named("item".into()),
            value: Some("a".into()),
        };
        assert_eq!(caps.missing_summary(&hydra_cms()), None);
        assert_eq!(caps.missing_readout(&hydra_cms(), &item), None);
        assert_eq!(
            caps.missing_readout(&hydra_cms(), &SketchStatistic::TopK { k: 1 })
                .as_deref(),
            Some("deployment lacks a HydraCms TopK readout")
        );
        let per_group = FieldDataType::Sketch(
            SketchKind::new(
                SketchAlgorithm::Cms,
                SketchParams::Cms {
                    width: 64,
                    depth: 4,
                },
            ),
            GroupingStrategy::PerSubpopulationInstance,
        );
        assert_eq!(
            caps.missing_summary(&per_group).as_deref(),
            Some("deployment lacks a Cms summary")
        );
    }
}
