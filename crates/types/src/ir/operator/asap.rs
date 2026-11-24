//! ASAP operators: summary-state construction, state operations and evaluations.
//! The summary type (family and algorithm) is an operator argument; θ is a
//! candidate's logical property.

use std::rc::Rc;

use serde::{Deserialize, Serialize};

use super::input::{InputOrTemporaryBuild, OperatorInput};
use super::node::{Operator, OperatorNode, OperatorResultKind};
use super::non_asap::NonASAPOp;
use super::tumbling_window_chain::is_time_indexed_state;
use crate::ir::operator::arguments::Reduction;
use crate::ir::operator::maintained_population::{MaintainedPopulation, PopulationStatistic};
use crate::ir::schema::aggregate_schema::{aggregate_output, OutputColumnSource};
use crate::ir::schema::{ColumnId, DataType, Field, FieldDataType, Schema};
use crate::ir::SchemaDerivationError;
use crate::summary_type::SummaryTypeCapability;
use crate::summary_type::{CounterReadout, SketchStatistic, SummaryUpdate};

/// Why an ASAP operator cannot be used yet.
pub const UNIMPLEMENTED_ASAP_OP: &str =
    "this ASAP operator is reserved: schema, accuracy, timing and export are not implemented";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
// `#[serde(default)]` fields would otherwise make serde require `C: Default`.
#[serde(bound(deserialize = "C: Deserialize<'de>"))]
pub enum ASAPOp<C = Rc<OperatorNode>> {
    /// Summary aggregation. Output: grouping columns + one field carrying
    /// partial summary state per group, typed `summary_type`.
    SummaryAgg {
        child: C,
        /// The summary type (family and algorithm) of the state. Never
        /// `FieldDataType::Plain`.
        summary_type: FieldDataType,
        input: SummaryUpdate,
        reduction: Reduction,
        #[serde(default)]
        filter: Option<crate::ir::scalar::Predicate<C>>,
    },
    /// Evaluate a query result from built summary state. Output is a
    /// row-shaped schema.
    SummaryEstimate {
        summary_input: C,
        query: SketchStatistic,
    },
    /// Read an exact accumulator's state as its finalized value: the
    /// maintenance-to-read boundary before query-time operators.
    FinalizeExactAccumulator {
        child: C,
        /// For a [`Counter`](crate::summary_type::ExactKind::Counter)
        /// state, which value it reads (`rate` or `increase`), so one
        /// counter state answers both; `None` for every other state.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        counter_readout: Option<CounterReadout>,
    },
    /// Maintain the full declared population, including membership changes.
    MaintainPopulation {
        child: C,
        population: MaintainedPopulation<OperatorNode>,
    },
    /// Read an aggregate or TopK prefix from the maintained population.
    EvaluatePopulation {
        child: C,
        evaluation: PopulationStatistic,
    },
    /// Merge compatible partial states of one family: every input state of
    /// a group into one state per group, or, with `group_by`, regrouped.
    /// Over a time-indexed state (a `TimeRange` over a
    /// [`SummaryTumblingWindowChain`](ASAPOp::SummaryTumblingWindowChain)), the windows it
    /// selects; there is no window-selection field.
    SummaryMerge {
        children: Vec<C>,
        /// A rollup (regrouping merge): the input's plain columns the
        /// merged states are keyed by, in output order. Every input state
        /// whose values in these columns match goes into one output state,
        /// so states keyed by `(region, job)` merge into one per `job`.
        /// `None` keeps the input's groups.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        group_by: Option<Vec<ColumnId>>,
    },
    /// A chain of tumbling windows (concepts doc, section 3, "Chain
    /// groups"): the `SummaryAgg` with these `summary_type`, `input`,
    /// `reduction` and `filter`, built over each window
    /// `(k·w, (k+1)·w]` of `child`, `w` = `tumbling_window_width_ms`, on a
    /// grid anchored at the epoch. Each window is built when it ends. Output:
    /// one row per window and key, (window_end, keys, state), a state with
    /// a time index (`window_end`, column 0), unique by (window_end, keys).
    /// It has no range and no window count: a `TimeRange` (and
    /// `TimeShift`) over it selects the windows a query reads, and how many
    /// windows it retains is $h_v$, a Stage 3 choice.
    // Written as `PaneChain` before the rename; the alias reads those documents.
    #[serde(alias = "PaneChain")]
    SummaryTumblingWindowChain {
        child: C,
        summary_type: FieldDataType,
        input: SummaryUpdate,
        reduction: Reduction,
        #[serde(default)]
        filter: Option<crate::ir::scalar::Predicate<C>>,
        #[serde(alias = "pane_width_ms")]
        tumbling_window_width_ms: u64,
    },
    /// A chain of wider tumbling windows built from a narrower chain
    /// `child` (a `SummaryTumblingWindowChain` or another
    /// `SummaryMergeWindows`, whose width divides
    /// `tumbling_window_width_ms`): its window `(k·w, (k+1)·w]`, `w` =
    /// `tumbling_window_width_ms`, is the merge of `child`'s windows inside
    /// it, per key. Same output schema as `child` (window_end, keys, state).
    /// A candidate of the group `SummaryTumblingWindowChain[w](X)` of the
    /// same build: it holds the same states as that chain built from the
    /// rows, for a merge per narrower state instead of an update per row.
    /// Example: 5 min windows from 1 min windows, five merges per key each.
    SummaryMergeWindows {
        child: C,
        tumbling_window_width_ms: u64,
    },
    /// The raw sample rows of `(T − range, T]` (for `Instant`, the newest
    /// sample per series within `range`) decoded from the windows of the
    /// compressed-samples chain under `child` (through any `TimeShift`s,
    /// which carry an offset) that overlap the range; with
    /// `reads_unsealed_window`, the chain's open window too, else only its
    /// sealed windows. Its schema and rows are those of `TimeRange(range,
    /// kind) ← Scan` over the chain's source with the label columns
    /// `labels` (each derived from the series identity), placed between the
    /// value and the identity (design doc `compressed-sample-windows.md`).
    DecodeSamplesInRange {
        child: C,
        range: std::time::Duration,
        kind: crate::ir::TimeRangeKind,
        labels: Vec<Field>,
        reads_unsealed_window: bool,
    },
    // ── Reserved: migrated but unimplemented ──
    SummarySubtract {
        left: C,
        right: C,
    },
    SummaryDelete {
        summary_input: C,
        key: ColumnId,
    },
    SummaryJoin {
        outer: C,
        inner: C,
        key: ColumnId,
        summary_type: FieldDataType,
    },
    Extension {
        child: C,
        name: String,
    },
}

impl<C> ASAPOp<C> {
    pub(crate) fn children(&self) -> Vec<&C> {
        use ASAPOp::*;
        match self {
            SummaryAgg { child, filter, .. } | SummaryTumblingWindowChain { child, filter, .. } => {
                let mut inputs = vec![child];
                if let Some(filter) = filter {
                    inputs.extend(filter.0.operator_refs());
                }
                inputs
            }
            FinalizeExactAccumulator { child, .. }
            | MaintainPopulation { child, .. }
            | EvaluatePopulation { child, .. }
            | SummaryMergeWindows { child, .. }
            | DecodeSamplesInRange { child, .. }
            | Extension { child, .. } => vec![child],
            SummaryEstimate { summary_input, .. } | SummaryDelete { summary_input, .. } => {
                vec![summary_input]
            }
            SummarySubtract { left, right } => vec![left, right],
            SummaryJoin { outer, inner, .. } => vec![outer, inner],
            SummaryMerge { children, .. } => children.iter().collect(),
        }
    }

    /// `f` may change the reference type, e.g. from `Rc<OperatorNode>` to a
    /// node id.
    pub(crate) fn map_children<D>(&self, mut f: impl FnMut(&C) -> D) -> ASAPOp<D> {
        use ASAPOp::*;
        match self {
            SummaryAgg {
                child,
                summary_type: family,
                input,
                reduction,
                filter,
            } => SummaryAgg {
                child: f(child),
                summary_type: family.clone(),
                input: input.clone(),
                reduction: reduction.clone(),
                filter: filter
                    .as_ref()
                    .map(|p| crate::ir::scalar::Predicate(p.0.map_operator_refs(&mut f))),
            },
            SummaryTumblingWindowChain {
                child,
                summary_type: family,
                input,
                reduction,
                filter,
                tumbling_window_width_ms,
            } => SummaryTumblingWindowChain {
                child: f(child),
                summary_type: family.clone(),
                input: input.clone(),
                reduction: reduction.clone(),
                filter: filter
                    .as_ref()
                    .map(|p| crate::ir::scalar::Predicate(p.0.map_operator_refs(&mut f))),
                tumbling_window_width_ms: *tumbling_window_width_ms,
            },
            SummaryMergeWindows {
                child,
                tumbling_window_width_ms,
            } => SummaryMergeWindows {
                child: f(child),
                tumbling_window_width_ms: *tumbling_window_width_ms,
            },
            SummaryEstimate {
                summary_input,
                query,
            } => SummaryEstimate {
                summary_input: f(summary_input),
                query: query.clone(),
            },
            FinalizeExactAccumulator {
                child,
                counter_readout,
            } => FinalizeExactAccumulator {
                child: f(child),
                counter_readout: *counter_readout,
            },
            MaintainPopulation { child, population } => MaintainPopulation {
                child: f(child),
                population: population.clone(),
            },
            EvaluatePopulation { child, evaluation } => EvaluatePopulation {
                child: f(child),
                evaluation: evaluation.clone(),
            },
            SummaryMerge { children, group_by } => SummaryMerge {
                children: children.iter().map(&mut f).collect(),
                group_by: group_by.clone(),
            },
            DecodeSamplesInRange {
                child,
                range,
                kind,
                labels,
                reads_unsealed_window,
            } => DecodeSamplesInRange {
                child: f(child),
                range: *range,
                kind: *kind,
                labels: labels.clone(),
                reads_unsealed_window: *reads_unsealed_window,
            },
            SummarySubtract { left, right } => SummarySubtract {
                left: f(left),
                right: f(right),
            },
            SummaryDelete { summary_input, key } => SummaryDelete {
                summary_input: f(summary_input),
                key: *key,
            },
            SummaryJoin {
                outer,
                inner,
                key,
                summary_type: family,
            } => SummaryJoin {
                outer: f(outer),
                inner: f(inner),
                key: *key,
                summary_type: family.clone(),
            },
            Extension { child, name } => Extension {
                child: f(child),
                name: name.clone(),
            },
        }
    }

    pub(crate) fn kind_name(&self) -> &'static str {
        use ASAPOp::*;
        match self {
            SummaryAgg { .. } => "SummaryAgg",
            SummaryEstimate { .. } => "SummaryEstimate",
            FinalizeExactAccumulator { .. } => "FinalizeExactAccumulator",
            MaintainPopulation { .. } => "MaintainPopulation",
            EvaluatePopulation { .. } => "EvaluatePopulation",
            SummaryMerge { .. } => "SummaryMerge",
            SummaryTumblingWindowChain { .. } => "SummaryTumblingWindowChain",
            SummaryMergeWindows { .. } => "SummaryMergeWindows",
            DecodeSamplesInRange { .. } => "DecodeSamplesInRange",
            SummarySubtract { .. } => "SummarySubtract",
            SummaryDelete { .. } => "SummaryDelete",
            SummaryJoin { .. } => "SummaryJoin",
            Extension { .. } => "Extension",
        }
    }
}

impl<I: OperatorInput> ASAPOp<I> {
    fn unimplemented() -> SchemaDerivationError {
        SchemaDerivationError::InvalidScalarSignature(UNIMPLEMENTED_ASAP_OP.into())
    }

    /// The summary state this operator produces, if it produces state.
    pub fn produced_state(&self) -> Option<&FieldDataType> {
        match self {
            ASAPOp::SummaryAgg {
                summary_type: family,
                ..
            }
            | ASAPOp::SummaryTumblingWindowChain {
                summary_type: family,
                ..
            }
            | ASAPOp::SummaryJoin {
                summary_type: family,
                ..
            } => Some(family),
            ASAPOp::SummaryMerge { children, .. } => children
                .first()
                .and_then(|child| child.schema().summary_state_type()),
            ASAPOp::SummaryMergeWindows { child, .. } => child.schema().summary_state_type(),
            _ => None,
        }
    }

    /// Output schema derived from the operator and its children. Summary
    /// planning may retain a more specific schema (evaluation column naming)
    /// through [`OperatorNode::with_schema`]; all structural metadata must
    /// still agree with this derivation.
    pub(crate) fn output_schema(&self) -> Result<Schema, SchemaDerivationError> {
        use ASAPOp::*;
        Ok(match self {
            SummaryAgg {
                child,
                summary_type: family,
                reduction,
                ..
            } => summary_agg_layout(child.schema(), family, reduction)?.0,
            SummaryTumblingWindowChain {
                child,
                summary_type: family,
                reduction,
                ..
            } => chain_schema(child.schema(), family, reduction)?,
            SummaryMergeWindows { child, .. } => {
                self.validate_inputs()?;
                child.schema().clone()
            }
            SummaryEstimate {
                summary_input,
                query,
            } => {
                let input = summary_input.schema();
                let (name, mut dtype) = match query {
                    SketchStatistic::Quantile { .. } => ("quantile", DataType::Float64),
                    SketchStatistic::Cardinality => ("cardinality", DataType::Int64),
                    SketchStatistic::PointCount { .. } => ("count", DataType::Int64),
                    SketchStatistic::FrequencyL2 => ("frequency_l2", DataType::Float64),
                    SketchStatistic::FrequencyEntropy => ("frequency_entropy", DataType::Float64),
                    SketchStatistic::TopK { .. } => ("topk", DataType::Utf8),
                };
                if matches!(
                    summary_build(summary_input),
                    Some(
                        SummaryAgg {
                            reduction: Reduction::PerEntity,
                            ..
                        } | SummaryTumblingWindowChain {
                            reduction: Reduction::PerEntity,
                            ..
                        }
                    )
                ) && dtype == DataType::Int64
                {
                    dtype = DataType::Float64;
                }
                if let SketchStatistic::TopK { .. } = query {
                    if let Some(schema) = topk_estimate_schema(summary_input) {
                        return Ok(schema);
                    }
                }
                let nullable = matches!(query, SketchStatistic::Quantile { .. })
                    && quantile_may_be_empty(summary_input);
                let mut schema = input.clone();
                for field in &mut schema.fields {
                    if !field.is_plain() {
                        *field = Field::plain(name, dtype.clone(), nullable);
                    }
                }
                schema
            }
            FinalizeExactAccumulator {
                child,
                counter_readout,
            } => {
                // A rollup finalizes as the coarse build it stands for.
                if let Some((build, schema)) = rolled_up_build(child) {
                    return finalized_schema(Some(&build), &schema, *counter_readout);
                }
                // The windows a `TimeRange` over a chain selects, merged,
                // finalize as the chain's build over that range.
                finalized_schema(
                    summary_build(child).as_ref(),
                    child.schema(),
                    *counter_readout,
                )?
            }
            MaintainPopulation { child, .. } => child.schema().clone(),
            EvaluatePopulation { child, evaluation } => {
                use crate::ir::operator::maintained_population::PopulationInput;
                use crate::ir::operator::{AggIntent, GroupKeys};
                let Operator::ASAP(MaintainPopulation {
                    child: source,
                    population,
                }) = child.operator()
                else {
                    return Err(SchemaDerivationError::InvalidScalarSignature(
                        "population evaluation requires maintained membership".into(),
                    ));
                };
                if matches!(evaluation, PopulationStatistic::TopK { .. }) {
                    source.schema().clone()
                } else {
                    let (keys, column) = match &population.input {
                        PopulationInput::Rows {
                            grouping,
                            value_column,
                            ..
                        } => (grouping.clone(), Some(*value_column)),
                        PopulationInput::CurrentSeries(spec) => {
                            let keys = spec
                                .grouping
                                .iter()
                                .map(|name| {
                                    source.schema().column_id(name).ok_or_else(|| {
                                        SchemaDerivationError::InvalidScalarSignature(
                                            "population grouping column is absent".into(),
                                        )
                                    })
                                })
                                .collect::<Result<Vec<_>, _>>()?;
                            (
                                if spec.without {
                                    GroupKeys::without(keys)
                                } else {
                                    GroupKeys::by(keys)
                                },
                                None,
                            )
                        }
                    };
                    let measure = match evaluation {
                        PopulationStatistic::Quantile { q } => {
                            AggIntent::Quantile { q: *q, col: column }
                        }
                        PopulationStatistic::Sum => AggIntent::Sum { col: column },
                        PopulationStatistic::Count => AggIntent::Count,
                        PopulationStatistic::Average => AggIntent::Avg { col: column },
                        PopulationStatistic::TopK { .. } => unreachable!(),
                    };
                    crate::ir::NonASAPOp::Aggregate {
                        child: source.clone(),
                        reduction: Reduction::Reduce(keys),
                        measures: vec![measure],
                        output_names: vec![String::new()],
                        filters: vec![],
                        having: None,
                    }
                    .output_schema()?
                }
            }
            SummaryMerge {
                children, group_by, ..
            } => {
                self.validate_inputs()?;
                let input = children[0].schema();
                match (group_by, summary_build(&children[0])) {
                    (Some(keys), _) => regrouped_schema(input, keys)?,
                    // The windows a `TimeRange` over a chain selects merge
                    // into the state of the chain's build over that range,
                    // which has no window column (a `TimeRange` and a
                    // `TimeShift` keep their input's schema).
                    (
                        None,
                        Some(SummaryTumblingWindowChain {
                            child,
                            summary_type,
                            reduction,
                            ..
                        }),
                    ) => summary_agg_layout(child.schema(), &summary_type, &reduction)?.0,
                    (None, _) => input.clone(),
                }
            }
            DecodeSamplesInRange { child, labels, .. } => {
                self.validate_inputs()?;
                decoded_samples_schema(child, labels)?
            }
            SummarySubtract { .. }
            | SummaryDelete { .. }
            | SummaryJoin { .. }
            | Extension { .. } => return Err(Self::unimplemented()),
        })
    }

    pub(crate) fn output_kind(&self) -> OperatorResultKind {
        use ASAPOp::*;
        match self {
            SummaryAgg { .. }
            | SummaryTumblingWindowChain { .. }
            | SummaryMergeWindows { .. }
            | MaintainPopulation { .. }
            | SummaryMerge { .. }
            | SummarySubtract { .. }
            | SummaryDelete { .. }
            | SummaryJoin { .. }
            | Extension { .. } => OperatorResultKind::State,
            SummaryEstimate { summary_input, .. } => source_kind(summary_input),
            FinalizeExactAccumulator { child, .. } | EvaluatePopulation { child, .. } => {
                source_kind(child)
            }
            DecodeSamplesInRange { kind, .. } => match kind {
                crate::ir::TimeRangeKind::Instant => OperatorResultKind::InstantVector,
                crate::ir::TimeRangeKind::Range => OperatorResultKind::RangeVector,
            },
        }
    }

    /// Local input-contract checks.
    pub(crate) fn validate_inputs(&self) -> Result<(), SchemaDerivationError> {
        use ASAPOp::*;
        let needs_state = |node: &I, what: &str| {
            if node.result_kind() != OperatorResultKind::State {
                Err(SchemaDerivationError::InvalidScalarSignature(format!(
                    "{what} requires summary state as input, got {:?}",
                    node.result_kind()
                )))
            } else {
                Ok(())
            }
        };
        match self {
            SummaryMerge { children, .. } => {
                let Some(first) = children.first() else {
                    return Err(SchemaDerivationError::InvalidScalarSignature(
                        "summary merge requires at least one state input".into(),
                    ));
                };
                // Matching state parameters and grouping positions are necessary;
                // matching names alone cannot prove two states compatible.
                if first
                    .schema()
                    .fields
                    .iter()
                    .filter(|field| !field.is_plain())
                    .count()
                    != 1
                {
                    return Err(SchemaDerivationError::InvalidScalarSignature(
                        "summary merge requires exactly one state column".into(),
                    ));
                }
                // Over a time-indexed state: the windows one `TimeRange`
                // over a chain, or one `TimeRangeCover` over levels,
                // selects, not the whole chain.
                if children.iter().any(is_time_indexed_state)
                    && !matches!(
                        children.as_slice(),
                        [child] if matches!(
                            child.operator(),
                            Operator::NonASAP(
                                NonASAPOp::TimeRange { .. } | NonASAPOp::TimeRangeCover { .. }
                            )
                        )
                    )
                {
                    return Err(SchemaDerivationError::InvalidScalarSignature(
                        "a summary merge over a time-indexed state reads one TimeRange over a \
                         chain or one TimeRangeCover"
                            .into(),
                    ));
                }
                for child in children {
                    needs_state(child, "SummaryMerge")?;
                    if child.schema() != first.schema() {
                        return Err(SchemaDerivationError::InvalidScalarSignature(
                            "summary merge inputs must have identical state and grouping schemas"
                                .into(),
                        ));
                    }
                }
                // Equal schemas cannot tell a KLL over `latency` from one over
                // `size`, nor prove the inputs disjoint; aggregate coverage (in
                // the memo) decides whether a structurally valid merge is
                // semantically valid.
                Ok(())
            }
            SummaryEstimate {
                summary_input,
                query,
            } => {
                needs_state(summary_input, "SummaryEstimate")?;
                // The capability table is the one source of which statistic
                // a summary type answers.
                let states: Vec<_> = summary_input
                    .schema()
                    .fields
                    .iter()
                    .filter(|f| !f.is_plain())
                    .collect();
                let valid = match states.as_slice() {
                    [field] => match &field.dtype {
                        summary_type @ FieldDataType::Sketch(..) => {
                            SummaryTypeCapability::of(summary_type).answers(query)
                        }
                        _ => false,
                    },
                    _ => false,
                };
                if !valid {
                    return Err(SchemaDerivationError::InvalidScalarSignature(
                        "evaluation does not match its summary type".into(),
                    ));
                }
                Ok(())
            }
            FinalizeExactAccumulator {
                child,
                counter_readout,
            } => {
                needs_state(child, "FinalizeExactAccumulator")?;
                if child
                    .schema()
                    .fields
                    .iter()
                    .all(|f| !matches!(f.dtype, FieldDataType::ExactAggregate(..)))
                {
                    return Err(SchemaDerivationError::InvalidScalarSignature(
                        "FinalizeExactAccumulator requires exact accumulator state".into(),
                    ));
                }
                // A counter state is read as `rate` or `increase`; no other
                // exact state takes a counter readout.
                if child.schema().fields.iter().any(|f| {
                    matches!(&f.dtype, FieldDataType::ExactAggregate(kind, _)
                        if kind.measure(None, *counter_readout).is_none())
                }) {
                    return Err(SchemaDerivationError::InvalidScalarSignature(
                        "a counter state's finalize needs a counter readout, and only it takes one"
                            .into(),
                    ));
                }
                Ok(())
            }
            EvaluatePopulation { child, evaluation } => {
                needs_state(child, "EvaluatePopulation")?;
                if !matches!(child.operator(), Operator::ASAP(MaintainPopulation { population, .. }) if population.supports(evaluation))
                {
                    return Err(SchemaDerivationError::InvalidScalarSignature(
                        "population evaluation requires compatible maintained membership".into(),
                    ));
                }
                Ok(())
            }
            SummaryTumblingWindowChain {
                child,
                tumbling_window_width_ms,
                ..
            } => {
                if *tumbling_window_width_ms == 0 {
                    return Err(SchemaDerivationError::InvalidScalarSignature(
                        "a tumbling-window chain needs a positive tumbling-window width".into(),
                    ));
                }
                // The chain is the range selector: it reads the series the
                // selector would, or a SQL table's rows for a trailing window.
                if !matches!(
                    child.result_kind(),
                    OperatorResultKind::InstantVector | OperatorResultKind::Relation
                ) {
                    return Err(SchemaDerivationError::InvalidScalarSignature(format!(
                        "a tumbling-window chain reads an instant vector or a relation, got {:?}",
                        child.result_kind()
                    )));
                }
                self.summary_agg_inputs()
            }
            SummaryAgg { .. } => self.summary_agg_inputs(),
            SummaryMergeWindows {
                child,
                tumbling_window_width_ms,
            } => {
                let narrower_width_ms = child.operator().chain_tumbling_window_width_ms();
                match narrower_width_ms {
                    Some(narrower_width_ms)
                        if narrower_width_ms > 0
                            && *tumbling_window_width_ms > narrower_width_ms
                            && tumbling_window_width_ms.is_multiple_of(narrower_width_ms) =>
                    {
                        Ok(())
                    }
                    _ => Err(SchemaDerivationError::InvalidScalarSignature(format!(
                        "SummaryMergeWindows of {tumbling_window_width_ms} ms needs a narrower \
                         chain whose width divides it, got {narrower_width_ms:?}"
                    ))),
                }
            }
            DecodeSamplesInRange { child, range, .. } => {
                if range.is_zero() {
                    return Err(SchemaDerivationError::InvalidScalarSignature(
                        "a decode of compressed samples needs a positive range".into(),
                    ));
                }
                if compressed_samples_chain_under(child).is_none() {
                    return Err(SchemaDerivationError::InvalidScalarSignature(
                        "a decode of compressed samples reads a compressed-samples chain, \
                         through any time shifts"
                            .into(),
                    ));
                }
                Ok(())
            }
            MaintainPopulation { child, population } => {
                if !population.matches_node(child) {
                    return Err(SchemaDerivationError::InvalidScalarSignature(
                        "population input differs from its membership contract".into(),
                    ));
                }
                Ok(())
            }
            _ => Err(Self::unimplemented()),
        }
    }

    /// The input checks of a `SummaryAgg`, which a `SummaryTumblingWindowChain` shares.
    fn summary_agg_inputs(&self) -> Result<(), SchemaDerivationError> {
        let (ASAPOp::SummaryAgg {
            child,
            summary_type: family,
            input,
            filter,
            ..
        }
        | ASAPOp::SummaryTumblingWindowChain {
            child,
            summary_type: family,
            input,
            filter,
            ..
        }) = self
        else {
            unreachable!("called on a summary build");
        };
        if family.is_plain() || child.result_kind() == OperatorResultKind::State {
            return Err(SchemaDerivationError::InvalidScalarSignature(
                "summary aggregation requires values and produces a state family".into(),
            ));
        }
        fn check(
            expr: &crate::summary_type::SummaryInputExpr,
            schema: &Schema,
        ) -> Result<(), SchemaDerivationError> {
            use crate::summary_type::SummaryInputExpr;
            match expr {
                SummaryInputExpr::Column(col) => {
                    crate::ir::scalar::resolve_column_ref(col, schema).map_err(|e| {
                        SchemaDerivationError::InvalidScalarSignature(e.to_string())
                    })?;
                }
                SummaryInputExpr::Tuple(items) => {
                    for item in items {
                        check(item, schema)?;
                    }
                }
                _ => {}
            }
            Ok(())
        }
        check(&input.weight, child.schema())?;
        if let Some(item) = &input.item {
            check(item, child.schema())?;
        }
        if let Some(filter) = filter {
            if filter.0.scalar_type(child.schema())?.0 != DataType::Bool {
                return Err(SchemaDerivationError::InvalidScalarSignature(
                    "summary filter must be boolean".into(),
                ));
            }
        }
        Ok(())
    }
}

/// The compressed-samples chain `input` is, or reads through `TimeShift`s:
/// its `child`'s schema (the raw rows it is built over). `None` for any
/// other input.
pub(crate) fn compressed_samples_chain_under<I: OperatorInput>(input: &I) -> Option<Schema> {
    match input.operator() {
        Operator::NonASAP(NonASAPOp::TimeShift { child, .. }) => {
            compressed_samples_chain_under(&child)
        }
        Operator::ASAP(ASAPOp::SummaryTumblingWindowChain {
            child,
            summary_type: FieldDataType::CompressedSamples,
            ..
        }) => Some(child.schema().clone()),
        _ => None,
    }
}

/// The schema of the raw rows a `DecodeSamplesInRange` over `chain`
/// outputs: the chain's input rows (time, value, series identity) with
/// `labels` inserted before the identity, keyed as the input is.
fn decoded_samples_schema<I: OperatorInput>(
    chain: &I,
    labels: &[Field],
) -> Result<Schema, SchemaDerivationError> {
    let mut schema = compressed_samples_chain_under(chain).ok_or_else(|| {
        SchemaDerivationError::InvalidScalarSignature(
            "a decode of compressed samples reads a compressed-samples chain".into(),
        )
    })?;
    let identity = schema
        .fields
        .iter()
        .position(|field| field.name == crate::ir::schema::PROMQL_SERIES_IDENTITY)
        .unwrap_or(schema.fields.len());
    let shift = |column: ColumnId| {
        if column >= identity {
            column + labels.len()
        } else {
            column
        }
    };
    schema.time_index = schema.time_index.map(shift);
    for key in &mut schema.unique_keys {
        for column in key.iter_mut() {
            *column = shift(*column);
        }
    }
    schema
        .fields
        .splice(identity..identity, labels.iter().cloned());
    Ok(schema)
}

/// The build a rollup (a `SummaryMerge` with `group_by` of one input)
/// stands for, with its schema: the input's build (one build over a merged
/// chain's window) grouped by the input columns the rollup's keys name.
/// `None` for any other input, or a rollup whose build does not derive.
fn rolled_up_build<I: OperatorInput>(
    state: &I,
) -> Option<(ASAPOp<InputOrTemporaryBuild<I>>, Schema)> {
    let Operator::ASAP(ASAPOp::SummaryMerge {
        children,
        group_by: Some(keys),
        ..
    }) = state.operator()
    else {
        return None;
    };
    let [input] = children.as_slice() else {
        return None;
    };
    let input = selected_from(input).unwrap_or_else(|| input.clone());
    // A build, or the windows a `TimeRange` over a chain selects, merged:
    // the chain's build over that range, whose schema is the build's over
    // the chain's input.
    let build = match (input.operator(), summary_build(&input)) {
        (Operator::ASAP(op @ ASAPOp::SummaryAgg { .. }), _)
        | (_, Some(op @ ASAPOp::SummaryTumblingWindowChain { .. })) => {
            op.map_children(|child| InputOrTemporaryBuild::Input(child.clone()))
        }
        _ => return None,
    };
    let (ASAPOp::SummaryAgg {
        child,
        summary_type,
        input: update,
        reduction,
        filter,
    }
    | ASAPOp::SummaryTumblingWindowChain {
        child,
        summary_type,
        input: update,
        reduction,
        filter,
        ..
    }) = build
    else {
        return None;
    };
    let mapped = keys
        .iter()
        .map(|&key| match &reduction {
            Reduction::Reduce(by) if !by.is_without() => by.keys().get(key).copied(),
            Reduction::PerEntity => Some(key),
            Reduction::Reduce(_) => None,
        })
        .collect::<Option<Vec<_>>>()?;
    let reduction = Reduction::by(mapped);
    let (schema, _) = summary_agg_layout(child.schema(), &summary_type, &reduction).ok()?;
    Some((
        ASAPOp::SummaryAgg {
            child,
            summary_type,
            input: update,
            reduction,
            filter,
        },
        schema,
    ))
}

/// The schema of a `SummaryAgg` (or `SummaryTumblingWindowChain`) of
/// `summary_type` by `reduction` over rows of schema `child`, and where each
/// of its columns comes from: the reduction's output columns, its one
/// measure's value column replaced by the state
/// (`OutputColumnSource::Measure`).
pub(crate) fn summary_agg_layout(
    child: &Schema,
    summary_type: &FieldDataType,
    reduction: &Reduction,
) -> Result<(Schema, Vec<OutputColumnSource>), SchemaDerivationError> {
    let (mut schema, sources) = aggregate_output(
        child,
        reduction,
        &[crate::ir::operator::AggIntent::Sum { col: None }],
        &[],
    )?;
    let index = sources
        .iter()
        .position(|source| matches!(source, OutputColumnSource::Measure { .. }))
        .ok_or_else(|| {
            SchemaDerivationError::InvalidScalarSignature(
                "a summary state has no value column".into(),
            )
        })?;
    schema.fields[index] = Field::new("state", summary_type.clone(), false);
    Ok((schema, sources))
}

/// The schema of a `SummaryTumblingWindowChain` of `summary_type` by `reduction`
/// over rows of schema `child`: a state with a time index, one row per
/// window and key, (window_end, keys…, state). `window_end` (column 0, the
/// time index) is computed, so it takes no name or table from `child`; the
/// build's own time column, which a per-entity build keeps, is dropped. The
/// unique key is (window_end, keys).
fn chain_schema(
    child: &Schema,
    summary_type: &FieldDataType,
    reduction: &Reduction,
) -> Result<Schema, SchemaDerivationError> {
    let (mut schema, _) = summary_agg_layout(child, summary_type, reduction)?;
    let time = child
        .time_index
        .and_then(|index| child.fields.get(index))
        .ok_or_else(|| {
            SchemaDerivationError::InvalidScalarSignature(
                "a tumbling-window chain reads rows with a time column".into(),
            )
        })?;
    if let Some(build_time) = schema.time_index.take() {
        schema.fields.remove(build_time);
    }
    schema
        .fields
        .insert(0, Field::new("window_end", time.dtype.clone(), false));
    schema.time_index = Some(0);
    schema.unique_keys = vec![(0..schema.fields.len())
        .filter(|&column| schema.fields[column].is_plain())
        .collect()];
    Ok(schema)
}

/// The schema `FinalizeExactAccumulator` derives over state of
/// `state_schema` that `build` (the state's `SummaryAgg`, if any) builds:
/// each exact aggregate replaced by the value it finalizes to, a counter
/// state read as `counter_readout`.
fn finalized_schema<J: OperatorInput>(
    build: Option<&ASAPOp<J>>,
    state_schema: &Schema,
    counter_readout: Option<CounterReadout>,
) -> Result<Schema, SchemaDerivationError> {
    let value_result = if let Some(
        ASAPOp::SummaryAgg {
            child: source,
            summary_type: FieldDataType::ExactAggregate(kind, _),
            input,
            reduction,
            filter,
            ..
        }
        | ASAPOp::SummaryTumblingWindowChain {
            child: source,
            summary_type: FieldDataType::ExactAggregate(kind, _),
            input,
            reduction,
            filter,
            ..
        },
    ) = build
    {
        use crate::summary_type::SummaryInputExpr;
        let column = match &input.weight {
            SummaryInputExpr::Column(col) => Some(
                crate::ir::scalar::column_resolution::resolve_column_ref(col, source.schema())
                    .map_err(|e| SchemaDerivationError::InvalidScalarSignature(e.to_string()))?,
            ),
            _ => None,
        };
        let aggregate_schema = crate::ir::NonASAPOp::Aggregate {
            child: source.clone(),
            reduction: reduction.clone(),
            measures: vec![kind.measure(column, counter_readout).ok_or_else(|| {
                SchemaDerivationError::InvalidScalarSignature(
                    "an exact state's finalize reads a counter readout only from a counter".into(),
                )
            })?],
            output_names: vec![String::new()],
            // A filtered SUM/MIN/MAX is NULL for a
            // group without a matching row.
            filters: vec![filter.clone()],
            having: None,
        }
        .output_schema()?;
        match reduction {
            Reduction::PerEntity => aggregate_schema
                .column_id("value")
                .and_then(|i| aggregate_schema.fields.get(i).cloned()),
            Reduction::Reduce(_) => aggregate_schema.fields.last().cloned(),
        }
    } else {
        None
    };
    let mut out = state_schema.clone();
    for f in &mut out.fields {
        if let FieldDataType::ExactAggregate(kind, _) = &f.dtype {
            if let Some(result) = &value_result {
                // The finalized value replaces the aggregate it
                // realizes, so it takes that aggregate's column.
                f.name = result.name.clone();
                f.dtype = result.dtype.clone();
                f.nullable = result.nullable;
            } else {
                f.dtype = FieldDataType::Plain(finalized_data_type(kind));
            }
        }
    }
    Ok(out)
}

/// The schema of a regrouping `SummaryMerge` over states of schema `input`:
/// the key columns `keys`, in order, then the state column. Each key must
/// be a distinct plain column of `input` other than its time column.
fn regrouped_schema(input: &Schema, keys: &[ColumnId]) -> Result<Schema, SchemaDerivationError> {
    let invalid = |message: &str| SchemaDerivationError::InvalidScalarSignature(message.into());
    let mut fields = Vec::with_capacity(keys.len() + 1);
    for (position, &key) in keys.iter().enumerate() {
        let field = input
            .fields
            .get(key)
            .ok_or_else(|| invalid("a regrouping merge key is not an input column"))?;
        if !field.is_plain() || input.time_index == Some(key) || keys[..position].contains(&key) {
            return Err(invalid(
                "a regrouping merge keys by distinct plain input columns other than time",
            ));
        }
        fields.push(field.clone());
    }
    fields.extend(input.fields.iter().find(|field| !field.is_plain()).cloned());
    // As an aggregate's: one row per key value, and no other column.
    Ok(Schema {
        fields,
        time_index: None,
        unique_keys: if keys.is_empty() {
            Vec::new()
        } else {
            vec![(0..keys.len()).collect()]
        },
        closed: true,
    })
}

/// The plain value an exact accumulator finalizes to.
fn finalized_data_type(kind: &crate::summary_type::ExactKind) -> DataType {
    use crate::summary_type::ExactKind;
    match kind {
        ExactKind::Count => DataType::Int64,
        _ => DataType::Float64,
    }
}

/// The states `node` selects from when it keeps whole summary states (its
/// output is state): a `Filter` by their plain columns, or a `TimeRange` or
/// `TimeShift` over a time-indexed state, which selects windows. `None` for
/// any other input.
pub fn selected_from<I: OperatorInput>(node: &I) -> Option<I> {
    match node.operator() {
        Operator::NonASAP(
            NonASAPOp::Filter { child, .. }
            | NonASAPOp::TimeRange { child, .. }
            | NonASAPOp::TimeShift { child, .. },
        ) if node.result_kind() == OperatorResultKind::State => Some(child),
        // A cover selects windows of its narrowest level (and of wider
        // levels of the same build).
        Operator::NonASAP(NonASAPOp::TimeRangeCover { levels, .. }) => levels.last().cloned(),
        _ => None,
    }
}

/// The build whose state this summary state carries: a `SummaryAgg` or
/// `SummaryTumblingWindowChain` itself, the chain under the `TimeRange` and
/// `TimeShift`s that select its windows, or the first input's build under
/// `SummaryMerge`s (merged inputs have one definition, which summary
/// coverage checks). `None` under a regrouping merge, whose states no build
/// carries (its coverage's definition is the build it stands for), and for
/// any other input.
pub fn summary_build<I: OperatorInput>(state: &I) -> Option<ASAPOp<I>> {
    match state.operator() {
        Operator::ASAP(
            op @ (ASAPOp::SummaryAgg { .. } | ASAPOp::SummaryTumblingWindowChain { .. }),
        ) => Some(op),
        Operator::ASAP(ASAPOp::SummaryMerge {
            children,
            group_by: None,
            ..
        }) => summary_build(children.first()?),
        Operator::NonASAP(
            NonASAPOp::TimeRange { child, .. } | NonASAPOp::TimeShift { child, .. },
        )
        | Operator::ASAP(ASAPOp::SummaryMergeWindows { child, .. }) => summary_build(&child),
        // Every level of a cover holds states of one build.
        Operator::NonASAP(NonASAPOp::TimeRangeCover { levels, .. }) => {
            summary_build(levels.last()?)
        }
        _ => None,
    }
}

/// The summary type a build operator builds: a `SummaryAgg`'s or a
/// `SummaryTumblingWindowChain`'s `summary_type`; `None` for every other
/// operator.
pub fn summary_type_of_operator<I>(operator: &Operator<I>) -> Option<&FieldDataType> {
    match operator {
        Operator::ASAP(
            ASAPOp::SummaryAgg { summary_type, .. }
            | ASAPOp::SummaryTumblingWindowChain { summary_type, .. },
        ) => Some(summary_type),
        _ => None,
    }
}

/// The category of the rows `node` stands for once evaluated: its own result
/// kind, or, for summary states, the kind of the rows they were built from
/// ([`source_kind`]). Sum states built over a SQL table `t` and then
/// subtracted or merged stand for a `Relation`, so a global rollup of them
/// has SQL's one row over no input rows; built over a PromQL series `m` they
/// stand for an `InstantVector`, so it has no sample over an empty vector.
pub fn evaluated_rows_kind<I: OperatorInput>(node: &I) -> OperatorResultKind {
    match node.result_kind() {
        OperatorResultKind::State => source_kind(node),
        kind => kind,
    }
}

/// The category of the values a evaluation of `state` produces: the category
/// of the relational input the state was built from.
fn source_kind<I: OperatorInput>(node: &I) -> OperatorResultKind {
    if let Some(states) = selected_from(node) {
        return source_kind(&states);
    }
    match node.operator() {
        Operator::ASAP(op) => match op.children().first() {
            Some(child) => source_kind(*child),
            None => OperatorResultKind::Relation,
        },
        Operator::NonASAP(_) => match node.result_kind() {
            OperatorResultKind::RangeVector => OperatorResultKind::InstantVector,
            OperatorResultKind::State => OperatorResultKind::Relation,
            other => other,
        },
    }
}

/// The rows a heap sketch's top-k estimate returns: per state row, its `k`
/// heaviest items, each as the state's partition columns, then the item
/// columns of the build's update (the inner group labels, or the PromQL
/// series identity), then the estimated weight, `topk` (Float64). `None`
/// when `state` is not a keyed build whose item columns resolve in its
/// input.
fn topk_estimate_schema<I: OperatorInput>(state: &I) -> Option<Schema> {
    // A rollup reads items as the coarse build it stands for.
    match rolled_up_build(state) {
        Some((build, _)) => topk_schema_of_build(&build, state.schema()),
        None => topk_schema_of_build(&summary_build(state)?, state.schema()),
    }
}

/// [`topk_estimate_schema`] of state of `state_schema` built by `build`.
fn topk_schema_of_build<J: OperatorInput>(
    build: &ASAPOp<J>,
    state_schema: &Schema,
) -> Option<Schema> {
    use crate::summary_type::SummaryInputExpr;
    let (ASAPOp::SummaryAgg { child, input, .. }
    | ASAPOp::SummaryTumblingWindowChain { child, input, .. }) = build
    else {
        return None;
    };
    let child_schema = child.schema();
    let resolve = |column: &crate::ir::scalar::ColumnRef| {
        crate::ir::scalar::resolve_column_ref(column, child_schema).ok()
    };
    let columns = match input.item.as_ref()? {
        SummaryInputExpr::Column(column) => vec![resolve(column)?],
        SummaryInputExpr::Tuple(items) => items
            .iter()
            .map(|item| match item {
                SummaryInputExpr::Column(column) => resolve(column),
                _ => None,
            })
            .collect::<Option<Vec<_>>>()?,
        SummaryInputExpr::EntityIdentity(crate::summary_type::EntityIdentity::PromqlLabelSet {
            excluding,
        }) => {
            let excluded: Vec<_> = excluding.iter().filter_map(resolve).collect();
            let value = resolve(&crate::ir::scalar::ColumnRef::SampleValue);
            (0..child_schema.fields.len())
                .filter(|index| {
                    Some(*index) != child_schema.time_index
                        && Some(*index) != value
                        && !excluded.contains(index)
                })
                .collect()
        }
        _ => return None,
    };
    // A column/tuple item is completely represented by these output columns.
    // An open PromQL label-set item may also contain unlisted labels, so its
    // visible columns alone cannot certify uniqueness.
    let complete_item_identity = matches!(
        input.item.as_ref()?,
        SummaryInputExpr::Column(_) | SummaryInputExpr::Tuple(_)
    ) || crate::ir::schema::topk::item_identity_is_complete(
        child_schema,
        columns.iter().copied(),
    );
    let mut fields: Vec<Field> = state_schema
        .fields
        .iter()
        .filter(|field| field.is_plain())
        .cloned()
        .collect();
    for index in columns {
        if !fields
            .iter()
            .any(|field| field.name == child_schema.fields[index].name)
        {
            fields.push(child_schema.fields[index].clone());
        }
    }
    // One row per (partition, item), not one row per partition. Preserve this
    // key through the ranked-row projection used by the logical TopK result.
    fields.push(Field::plain("topk", DataType::Float64, false));
    let unique_keys = if complete_item_identity {
        crate::ir::schema::topk::ranked_rows_unique_keys(fields.len(), fields.len() - 1)
    } else {
        Vec::new()
    };
    Some(Schema {
        fields,
        unique_keys,
        ..state_schema.clone()
    })
}

/// Whether a quantile read from `state` can have no value to rank, and so is
/// NULL as the SQL aggregate it realizes is: the `SummaryAgg` under it (seen
/// through `SummaryMerge`) is filtered, or, over a relation, is global (zero
/// rows give one empty state) or summarizes a nullable column (a group of
/// NULLs gives an empty state).
fn quantile_may_be_empty<I: OperatorInput>(state: &I) -> bool {
    match state.operator() {
        Operator::ASAP(ASAPOp::SummaryMerge {
            children, group_by, ..
        }) => {
            // A global rollup must also represent the empty input population,
            // even when every existing grouped state is nonempty.
            (source_kind(state) == OperatorResultKind::Relation
                && group_by.as_ref().is_some_and(Vec::is_empty))
                || children.iter().any(quantile_may_be_empty)
        }
        Operator::ASAP(
            ASAPOp::SummaryAgg {
                child,
                input,
                reduction,
                filter,
                ..
            }
            | ASAPOp::SummaryTumblingWindowChain {
                child,
                input,
                reduction,
                filter,
                ..
            },
        ) => {
            if filter.is_some() {
                return true;
            }
            if child.result_kind() != OperatorResultKind::Relation {
                return false;
            }
            let global = matches!(reduction, Reduction::Reduce(by) if by.keys().is_empty());
            let nullable = match &input.weight {
                crate::summary_type::SummaryInputExpr::Column(column) => {
                    crate::ir::scalar::resolve_column_ref(column, child.schema())
                        .ok()
                        .and_then(|index| child.schema().fields.get(index))
                        .is_some_and(|field| field.nullable)
                }
                _ => false,
            };
            global || nullable
        }
        // Selecting whole states empties none of them.
        _ => selected_from(state).is_some_and(|states| quantile_may_be_empty(&states)),
    }
}
