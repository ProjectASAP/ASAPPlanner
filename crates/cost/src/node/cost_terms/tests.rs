//! Tests of [`CostConstant`]: its values, its coverage of the per-unit
//! constants and its JSON names.

use std::collections::BTreeSet;

use super::*;

/// Each constant's value is the per-unit constant of the same name in
/// `operations_per_unit.rs`. Example:
/// `OperationsPerGroupPerRawBatch` is 170 operations per group visit.
#[test]
fn cost_constants_hold_the_built_in_values() {
    for (constant, operations_per_unit) in [
        (CostConstant::OperationsPerGroupPerRawBatch, 170.0),
        (CostConstant::KllUpdateOperationsPerRow, 84.0),
        (CostConstant::TimeRangeOperationsPerRow, 11.0),
        (CostConstant::OutputRowOperationsPerGroup, 2_500.0),
        (CostConstant::KllMergeOperationsPerOutputGroup, 19_000.0),
    ] {
        assert_eq!(
            constant.operations_per_unit(),
            operations_per_unit,
            "{constant:?}"
        );
    }
    // Every constant, against the file's text.
    let defined = defined_constants();
    assert_eq!(CostConstant::ALL.len(), defined.len());
    for constant in CostConstant::ALL {
        let name = format!("{constant:?}");
        let (_, value) = defined
            .iter()
            .find(|(defined_name, _)| *defined_name == name)
            .unwrap_or_else(|| panic!("{name} has no constant in operations_per_unit.rs"));
        assert_eq!(constant.operations_per_unit(), *value, "{name}");
    }
}

/// Every `const NAME: f64` of `operations_per_unit.rs`, except the four
/// exact-distinct shape parameters (which shape units, not charge them),
/// has a `CostConstant` named in UpperCamelCase, and every `CostConstant`
/// has such a constant: adding a constant without a variant fails here.
#[test]
fn every_per_unit_constant_has_a_cost_constant() {
    let defined: BTreeSet<String> = defined_constants()
        .into_iter()
        .map(|(name, _)| name)
        .collect();
    let variants: BTreeSet<String> = CostConstant::ALL
        .iter()
        .map(|constant| format!("{constant:?}"))
        .collect();
    assert_eq!(defined.len(), 37, "{defined:?}");
    assert_eq!(
        defined.difference(&variants).collect::<Vec<_>>(),
        Vec::<&String>::new(),
        "constants without a CostConstant"
    );
    assert_eq!(
        variants.difference(&defined).collect::<Vec<_>>(),
        Vec::<&String>::new(),
        "CostConstants without a constant"
    );
}

/// A constant's JSON name is its snake_case name; a name that is not a
/// constant is refused.
#[test]
fn cost_constant_json_names_are_snake_case() {
    for (json, constant) in [
        (
            "\"operations_per_group_per_raw_batch\"",
            CostConstant::OperationsPerGroupPerRawBatch,
        ),
        (
            "\"kll_update_operations_per_row\"",
            CostConstant::KllUpdateOperationsPerRow,
        ),
    ] {
        assert_eq!(
            serde_json::from_str::<CostConstant>(json).unwrap(),
            constant
        );
        assert_eq!(serde_json::to_string(&constant).unwrap(), json);
    }
    assert!(serde_json::from_str::<CostConstant>("\"operations_per_group\"").is_err());
}

/// The JSON name of every constant, in declaration order: the approx-bench
/// side reads these keys, so renaming a variant must fail here.
#[test]
fn cost_constant_json_names_are_the_full_snake_case_list() {
    let expected: [&str; 37] = [
        "time_range_operations_per_row",
        "operations_per_group_per_raw_batch",
        "chain_operations_per_arriving_row",
        "hash_aggregate_operations_per_row",
        "hash_aggregate_operations_per_further_key",
        "aggregate_measure_operations_per_row",
        "sort_operations_per_row_per_level",
        "ddsketch_update_operations_per_row",
        "kll_update_operations_per_row",
        "exact_update_operations_per_row",
        "counter_update_operations_per_row",
        "carried_label_operations_per_row_per_label",
        "kmv_update_operations_per_row",
        "kmv_operations_per_compared_hash",
        "exact_distinct_operations_per_row",
        "exact_distinct_operations_per_value_doubling",
        "exact_distinct_operations_per_row_at_full_value_miss",
        "exact_distinct_operations_per_row_at_full_pair_miss",
        "exact_distinct_operations_per_inserted_value",
        "exact_distinct_operations_per_inserted_pair",
        "sketch_update_operations_per_counter",
        "exact_merge_operations_per_state",
        "extremum_merge_operations_per_state",
        "counter_merge_operations_per_state",
        "ddsketch_merge_operations_per_state",
        "kll_merge_operations_per_state",
        "kll_merge_operations_per_input_byte",
        "kll_merge_operations_per_merged_byte",
        "merge_operations_per_output_group",
        "sketch_merge_operations_per_byte",
        "kll_estimate_operations_per_byte",
        "kll_estimate_operations_per_unsorted_item",
        "sketch_estimate_operations_per_group",
        "output_row_operations_per_group",
        "sketch_chain_emit_operations_per_state",
        "exact_chain_emit_operations_per_state",
        "kll_merge_operations_per_output_group",
    ];
    let actual: Vec<String> = CostConstant::ALL
        .iter()
        .map(|constant| {
            serde_json::to_string(constant)
                .unwrap()
                .trim_matches('"')
                .to_string()
        })
        .collect();
    assert_eq!(actual, expected);
}

/// The exact-distinct cache and size parameters: not operations per unit.
const SHAPE_PARAMETERS: [&str; 4] = [
    "EXACT_DISTINCT_FEWEST_VALUES",
    "EXACT_DISTINCT_MOST_VALUES",
    "EXACT_DISTINCT_VALUES_IN_CACHE",
    "EXACT_DISTINCT_PAIRS_IN_CACHE",
];

/// Each `const NAME: f64 = value;` of `operations_per_unit.rs` but the
/// shape parameters, as (UpperCamelCase name, value).
fn defined_constants() -> Vec<(String, f64)> {
    include_str!("../operations_per_unit.rs")
        .lines()
        .filter_map(|line| {
            let (_, declaration) = line.split_once("const ")?;
            let (name, value) = declaration.split_once(": f64 = ")?;
            let value: f64 = value.trim_end_matches(';').replace('_', "").parse().ok()?;
            Some((name.trim().to_string(), value))
        })
        .filter(|(name, _)| !SHAPE_PARAMETERS.contains(&name.as_str()))
        .map(|(name, value)| {
            let upper_camel = name
                .split('_')
                .map(|word| {
                    let mut letters = word.chars();
                    letters.next().map_or(String::new(), |first| {
                        first.to_string() + &letters.as_str().to_lowercase()
                    })
                })
                .collect();
            (upper_camel, value)
        })
        .collect()
}
