//! One node's formula CPU operations per execution, split into named
//! per-unit constants times the units of work each multiplies, plus the
//! operations no named constant charges.
//!
//! Example: a per-series `max_over_time(latency[5m])` hash aggregate over
//! 300,000 rows of 1,000 series at 1 Hz visits each series' group once per
//! 10 s raw batch: 30,000 units of `OperationsPerGroupPerRawBatch` (170
//! operations each), beside 300,000 units of `HashAggregateOperationsPerRow`
//! (16), 300,000 of `AggregateMeasureOperationsPerRow` (4) and 1,000 of
//! `OutputRowOperationsPerGroup` (2,500): 13.6 M operations, the formula's
//! charge. A refit of one constant changes the charge by its units times the
//! change.
//!
//! The terms describe the formulas only: they do not depend on
//! `CostInputs::measured_operation_costs` or `frozen_calibration_factor`.

use std::collections::BTreeMap;

use asap_types::plan::physical_asap_dag::PhysicalASAPDAGNode;
use asap_types::workload::DataWorkload;
use serde::{Deserialize, Serialize};

use crate::node::analytical::AnalyticalCostError;
use crate::node::operations_per_unit as built_in;
use crate::node::usage::node_usage_and_cost_terms;
use crate::{CostInputs, ProducerOutput};

/// A named per-unit constant of the cost formulas: CPU operations per unit
/// of work, e.g. `OperationsPerGroupPerRawBatch` (170 per group visit).
/// JSON: snake_case, e.g. `"operations_per_group_per_raw_batch"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CostConstant {
    TimeRangeOperationsPerRow,
    OperationsPerGroupPerRawBatch,
    ChainOperationsPerArrivingRow,
    HashAggregateOperationsPerRow,
    HashAggregateOperationsPerFurtherKey,
    AggregateMeasureOperationsPerRow,
    SortOperationsPerRowPerLevel,
    DdsketchUpdateOperationsPerRow,
    KllUpdateOperationsPerRow,
    ExactUpdateOperationsPerRow,
    CounterUpdateOperationsPerRow,
    CarriedLabelOperationsPerRowPerLabel,
    KmvUpdateOperationsPerRow,
    KmvUpdateOperationsPerComparedHash,
    KmvMergeOperationsPerHash,
    KmvMergeOperationsPerComparedHash,
    ExactDistinctOperationsPerRow,
    ExactDistinctOperationsPerValueDoubling,
    ExactDistinctOperationsPerRowAtFullValueMiss,
    ExactDistinctOperationsPerRowAtFullPairMiss,
    ExactDistinctOperationsPerInsertedValue,
    ExactDistinctOperationsPerInsertedPair,
    SketchUpdateOperationsPerCounter,
    CounterMatrixUpdateOperationsPerCounter,
    UnivmonUpdateOperationsPerCounter,
    CmsHeapOperationsPerEstimate,
    CmsHeapOperationsPerEstimatedCounter,
    CountSketchHeapOperationsPerEstimate,
    CountSketchHeapOperationsPerEstimatedCounter,
    SketchAllocationOperationsPerCounter,
    ExactMergeOperationsPerState,
    ExtremumMergeOperationsPerState,
    CounterMergeOperationsPerState,
    DdsketchMergeOperationsPerState,
    KllMergeOperationsPerState,
    KllMergeOperationsPerInputByte,
    KllMergeOperationsPerMergedByte,
    MergeOperationsPerOutputGroup,
    SketchMergeOperationsPerByte,
    CounterMatrixMergeOperationsPerState,
    CounterMatrixMergeOperationsPerCounter,
    HeapMergeOperationsPerResidentCounter,
    KllEstimateOperationsPerByte,
    KllEstimateOperationsPerUnsortedItem,
    SketchEstimateOperationsPerGroup,
    HeapReadOperationsPerGroup,
    HeapReadOperationsPerResidentCounter,
    OutputRowOperationsPerGroup,
    SketchChainEmitOperationsPerState,
    ExactChainEmitOperationsPerState,
    KllMergeOperationsPerOutputGroup,
}

impl CostConstant {
    /// Its built-in value, e.g. 170.0 for `OperationsPerGroupPerRawBatch`:
    /// the constant of the same name in `operations_per_unit`.
    pub fn operations_per_unit(self) -> f64 {
        match self {
            CostConstant::TimeRangeOperationsPerRow => built_in::TIME_RANGE_OPERATIONS_PER_ROW,
            CostConstant::OperationsPerGroupPerRawBatch => {
                built_in::OPERATIONS_PER_GROUP_PER_RAW_BATCH
            }
            CostConstant::ChainOperationsPerArrivingRow => {
                built_in::CHAIN_OPERATIONS_PER_ARRIVING_ROW
            }
            CostConstant::HashAggregateOperationsPerRow => {
                built_in::HASH_AGGREGATE_OPERATIONS_PER_ROW
            }
            CostConstant::HashAggregateOperationsPerFurtherKey => {
                built_in::HASH_AGGREGATE_OPERATIONS_PER_FURTHER_KEY
            }
            CostConstant::AggregateMeasureOperationsPerRow => {
                built_in::AGGREGATE_MEASURE_OPERATIONS_PER_ROW
            }
            CostConstant::SortOperationsPerRowPerLevel => {
                built_in::SORT_OPERATIONS_PER_ROW_PER_LEVEL
            }
            CostConstant::DdsketchUpdateOperationsPerRow => {
                built_in::DDSKETCH_UPDATE_OPERATIONS_PER_ROW
            }
            CostConstant::KllUpdateOperationsPerRow => built_in::KLL_UPDATE_OPERATIONS_PER_ROW,
            CostConstant::ExactUpdateOperationsPerRow => built_in::EXACT_UPDATE_OPERATIONS_PER_ROW,
            CostConstant::CounterUpdateOperationsPerRow => {
                built_in::COUNTER_UPDATE_OPERATIONS_PER_ROW
            }
            CostConstant::CarriedLabelOperationsPerRowPerLabel => {
                built_in::CARRIED_LABEL_OPERATIONS_PER_ROW_PER_LABEL
            }
            CostConstant::KmvUpdateOperationsPerRow => built_in::KMV_UPDATE_OPERATIONS_PER_ROW,
            CostConstant::KmvUpdateOperationsPerComparedHash => {
                built_in::KMV_UPDATE_OPERATIONS_PER_COMPARED_HASH
            }
            CostConstant::KmvMergeOperationsPerHash => built_in::KMV_MERGE_OPERATIONS_PER_HASH,
            CostConstant::KmvMergeOperationsPerComparedHash => {
                built_in::KMV_MERGE_OPERATIONS_PER_COMPARED_HASH
            }
            CostConstant::ExactDistinctOperationsPerRow => {
                built_in::EXACT_DISTINCT_OPERATIONS_PER_ROW
            }
            CostConstant::ExactDistinctOperationsPerValueDoubling => {
                built_in::EXACT_DISTINCT_OPERATIONS_PER_VALUE_DOUBLING
            }
            CostConstant::ExactDistinctOperationsPerRowAtFullValueMiss => {
                built_in::EXACT_DISTINCT_OPERATIONS_PER_ROW_AT_FULL_VALUE_MISS
            }
            CostConstant::ExactDistinctOperationsPerRowAtFullPairMiss => {
                built_in::EXACT_DISTINCT_OPERATIONS_PER_ROW_AT_FULL_PAIR_MISS
            }
            CostConstant::ExactDistinctOperationsPerInsertedValue => {
                built_in::EXACT_DISTINCT_OPERATIONS_PER_INSERTED_VALUE
            }
            CostConstant::ExactDistinctOperationsPerInsertedPair => {
                built_in::EXACT_DISTINCT_OPERATIONS_PER_INSERTED_PAIR
            }
            CostConstant::SketchUpdateOperationsPerCounter => {
                built_in::SKETCH_UPDATE_OPERATIONS_PER_COUNTER
            }
            CostConstant::ExactMergeOperationsPerState => {
                built_in::EXACT_MERGE_OPERATIONS_PER_STATE
            }
            CostConstant::ExtremumMergeOperationsPerState => {
                built_in::EXTREMUM_MERGE_OPERATIONS_PER_STATE
            }
            CostConstant::CounterMergeOperationsPerState => {
                built_in::COUNTER_MERGE_OPERATIONS_PER_STATE
            }
            CostConstant::DdsketchMergeOperationsPerState => {
                built_in::DDSKETCH_MERGE_OPERATIONS_PER_STATE
            }
            CostConstant::KllMergeOperationsPerState => built_in::KLL_MERGE_OPERATIONS_PER_STATE,
            CostConstant::KllMergeOperationsPerInputByte => {
                built_in::KLL_MERGE_OPERATIONS_PER_INPUT_BYTE
            }
            CostConstant::KllMergeOperationsPerMergedByte => {
                built_in::KLL_MERGE_OPERATIONS_PER_MERGED_BYTE
            }
            CostConstant::MergeOperationsPerOutputGroup => {
                built_in::MERGE_OPERATIONS_PER_OUTPUT_GROUP
            }
            CostConstant::SketchMergeOperationsPerByte => {
                built_in::SKETCH_MERGE_OPERATIONS_PER_BYTE
            }
            CostConstant::KllEstimateOperationsPerByte => {
                built_in::KLL_ESTIMATE_OPERATIONS_PER_BYTE
            }
            CostConstant::KllEstimateOperationsPerUnsortedItem => {
                built_in::KLL_ESTIMATE_OPERATIONS_PER_UNSORTED_ITEM
            }
            CostConstant::SketchEstimateOperationsPerGroup => {
                built_in::SKETCH_ESTIMATE_OPERATIONS_PER_GROUP
            }
            CostConstant::OutputRowOperationsPerGroup => built_in::OUTPUT_ROW_OPERATIONS_PER_GROUP,
            CostConstant::SketchChainEmitOperationsPerState => {
                built_in::SKETCH_CHAIN_EMIT_OPERATIONS_PER_STATE
            }
            CostConstant::ExactChainEmitOperationsPerState => {
                built_in::EXACT_CHAIN_EMIT_OPERATIONS_PER_STATE
            }
            CostConstant::KllMergeOperationsPerOutputGroup => {
                built_in::KLL_MERGE_OPERATIONS_PER_OUTPUT_GROUP
            }
            CostConstant::CounterMatrixUpdateOperationsPerCounter => {
                built_in::COUNTER_MATRIX_UPDATE_OPERATIONS_PER_COUNTER
            }
            CostConstant::UnivmonUpdateOperationsPerCounter => {
                built_in::UNIVMON_UPDATE_OPERATIONS_PER_COUNTER
            }
            CostConstant::CmsHeapOperationsPerEstimate => {
                built_in::CMS_HEAP_OPERATIONS_PER_ESTIMATE
            }
            CostConstant::CmsHeapOperationsPerEstimatedCounter => {
                built_in::CMS_HEAP_OPERATIONS_PER_ESTIMATED_COUNTER
            }
            CostConstant::CountSketchHeapOperationsPerEstimate => {
                built_in::COUNT_SKETCH_HEAP_OPERATIONS_PER_ESTIMATE
            }
            CostConstant::CountSketchHeapOperationsPerEstimatedCounter => {
                built_in::COUNT_SKETCH_HEAP_OPERATIONS_PER_ESTIMATED_COUNTER
            }
            CostConstant::SketchAllocationOperationsPerCounter => {
                built_in::SKETCH_ALLOCATION_OPERATIONS_PER_COUNTER
            }
            CostConstant::CounterMatrixMergeOperationsPerState => {
                built_in::COUNTER_MATRIX_MERGE_OPERATIONS_PER_STATE
            }
            CostConstant::CounterMatrixMergeOperationsPerCounter => {
                built_in::COUNTER_MATRIX_MERGE_OPERATIONS_PER_COUNTER
            }
            CostConstant::HeapMergeOperationsPerResidentCounter => {
                built_in::HEAP_MERGE_OPERATIONS_PER_RESIDENT_COUNTER
            }
            CostConstant::HeapReadOperationsPerGroup => built_in::HEAP_READ_OPERATIONS_PER_GROUP,
            CostConstant::HeapReadOperationsPerResidentCounter => {
                built_in::HEAP_READ_OPERATIONS_PER_RESIDENT_COUNTER
            }
        }
    }
}

/// One node's formula CPU operations per execution, as units of work per
/// named constant plus the operations no named constant charges.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct NodeCostTerms {
    /// Per constant, the units it multiplies in one execution, e.g. 30,000
    /// group visits for `OperationsPerGroupPerRawBatch`.
    pub units_per_constant: BTreeMap<CostConstant, f64>,
    /// Operations charged without a named constant: one per row a `Scan`
    /// reads, a predicate's operations per row, the rounding a hash
    /// aggregate applies to its per-row operations.
    pub operations_without_constant: f64,
    /// The formula's charge: these terms at the built-in values, added in
    /// the order the formula adds them. Floating-point addition depends on
    /// order, so this is the exact value the formula charges, where
    /// re-adding `units_per_constant` in key order may differ in the last
    /// bits. Example: `rows × (update + carried label) + visits × 170` adds
    /// the per-row operations before multiplying by the rows.
    operations_at_built_in_values: f64,
}

impl NodeCostTerms {
    /// `operations_without_constant + Σ units × value(constant)`; with
    /// [`CostConstant::operations_per_unit`] it is the formula's charge up
    /// to floating-point summation order.
    pub fn operations_with(&self, value: impl Fn(CostConstant) -> f64) -> f64 {
        self.operations_without_constant
            + self
                .units_per_constant
                .iter()
                .map(|(constant, units)| units * value(*constant))
                .sum::<f64>()
    }

    /// The operations at the built-in values: the formula's charge.
    pub(crate) fn operations(&self) -> f64 {
        self.operations_at_built_in_values
    }

    /// `units` of `constant` alone. Example: `of(TimeRangeOperationsPerRow,
    /// 300.0)` is a time range keeping 300 rows.
    pub(crate) fn of(constant: CostConstant, units: f64) -> Self {
        Self::default().plus(constant, units)
    }

    /// `operations` charged without a named constant alone, e.g. one per
    /// row a scan reads.
    pub(crate) fn without_constant(operations: f64) -> Self {
        Self {
            units_per_constant: BTreeMap::new(),
            operations_without_constant: operations,
            operations_at_built_in_values: operations,
        }
    }

    /// These terms and `units` more of `constant`; zero units add no entry.
    pub(crate) fn plus(mut self, constant: CostConstant, units: f64) -> Self {
        if units != 0.0 {
            *self.units_per_constant.entry(constant).or_insert(0.0) += units;
            self.operations_at_built_in_values += units * constant.operations_per_unit();
        }
        self
    }

    /// These terms and `other`'s.
    pub(crate) fn plus_terms(mut self, other: &NodeCostTerms) -> Self {
        for (constant, units) in &other.units_per_constant {
            *self.units_per_constant.entry(*constant).or_insert(0.0) += units;
        }
        self.operations_without_constant += other.operations_without_constant;
        self.operations_at_built_in_values += other.operations_at_built_in_values;
        self
    }

    /// These terms with `part`'s constants charging some of their
    /// operations without a constant: `part`'s units are added and the
    /// operations without a constant shrink by what those units charge, so
    /// the charge is unchanged. Example: a hash aggregate charges 300,000
    /// rows × 27 operations without a constant, its per-row operations
    /// 26.4 rounded up; attributing the 26.4 per row to their constants
    /// leaves the 180,000 operations of rounding without one.
    pub(crate) fn attributing_to_constants(mut self, part: &NodeCostTerms) -> Self {
        for (constant, units) in &part.units_per_constant {
            *self.units_per_constant.entry(*constant).or_insert(0.0) += units;
        }
        self.operations_without_constant -=
            part.operations_at_built_in_values - part.operations_without_constant;
        self
    }

    /// These terms `times` over: the terms per row times the rows.
    /// Example: one KLL update per row, times 1,000 rows, is 1,000 units
    /// of `KllUpdateOperationsPerRow`.
    pub(crate) fn times(mut self, times: f64) -> Self {
        for units in self.units_per_constant.values_mut() {
            *units *= times;
        }
        self.units_per_constant.retain(|_, units| *units != 0.0);
        self.operations_without_constant *= times;
        self.operations_at_built_in_values *= times;
        self
    }

    /// These terms shared over `parts`: the terms of a whole input spread
    /// over its rows. Example: 259,000 inserted values over 300,000 rows
    /// is 0.86 units of `ExactDistinctOperationsPerInsertedValue` per row.
    pub(crate) fn divided_by(mut self, parts: f64) -> Self {
        for units in self.units_per_constant.values_mut() {
            *units /= parts;
        }
        self.operations_without_constant /= parts;
        self.operations_at_built_in_values /= parts;
        self
    }
}

/// The terms of `node`'s formula CPU per execution, from the inputs
/// `node_resource_usage_and_cost` takes except the rate (which scales
/// executions, not units). Independent of
/// `settings.measured_operation_costs` and `frozen_calibration_factor`.
pub fn node_cost_terms<'a>(
    node: &'a PhysicalASAPDAGNode,
    producers: &[ProducerOutput<'a>],
    window_ms: Option<u64>,
    data: &DataWorkload,
    settings: &'a CostInputs,
) -> Result<NodeCostTerms, AnalyticalCostError> {
    // Its usage at one execution per second: the terms are computed before
    // the measured table or the calibration factor apply.
    node_usage_and_cost_terms(node, producers, (1, 1.0), window_ms, data, settings)
        .map(|(_, terms)| terms)
}

#[cfg(test)]
mod tests;
