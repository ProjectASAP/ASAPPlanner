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
    assert_eq!(every_constant().len(), defined.len());
    for constant in every_constant() {
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
    let variants: BTreeSet<String> = every_constant()
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
/// side reads these keys, so renaming a variant must fail here. Each
/// parses to a distinct constant that prints it back.
#[test]
fn cost_constant_json_names_are_the_full_snake_case_list() {
    let constants = every_constant();
    let distinct: BTreeSet<CostConstant> = constants.iter().copied().collect();
    assert_eq!(distinct.len(), JSON_NAMES.len());
    for (constant, json_name) in constants.iter().zip(JSON_NAMES) {
        assert_eq!(
            serde_json::to_string(constant).unwrap(),
            format!("\"{json_name}\"")
        );
    }
}

/// Every constant's JSON name, in declaration order.
const JSON_NAMES: [&str; 37] = [
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

/// Every constant, parsed from [`JSON_NAMES`]: with the count of the
/// constants of `operations_per_unit.rs`, a variant missing from the list
/// fails the tests above.
fn every_constant() -> Vec<CostConstant> {
    JSON_NAMES
        .iter()
        .map(|name| serde_json::from_value(serde_json::Value::from(*name)).unwrap())
        .collect()
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

/// The charge is the formula's own floating-point value, its terms added
/// in the order the formula adds them, so it does not move when the
/// terms are kept per constant: 0.1 compared hashes (0.5 each) and 0.3
/// merged bytes (0.43 each) per row, times 7 rows, charge
/// (0.1 × 0.5 + 0.3 × 0.43) × 7 = 1.253, where adding the per-constant
/// products 0.7 × 0.5 + 2.1 × 0.43 gives 1.2530000000000001.
#[test]
fn the_charge_adds_terms_in_the_formula_s_order() {
    let terms = NodeCostTerms::of(CostConstant::KmvOperationsPerComparedHash, 0.1)
        .plus(CostConstant::KllMergeOperationsPerMergedByte, 0.3)
        .times(7.0);
    assert_eq!(terms.operations(), (0.1 * 0.5 + 0.3 * 0.43) * 7.0);
    let by_constant = terms.operations_with(CostConstant::operations_per_unit);
    assert_ne!(by_constant, terms.operations());
    assert!((by_constant - terms.operations()).abs() <= 1e-12 * terms.operations());
}

/// Attributing per-row terms to their constants keeps the charge and
/// leaves the rounding without a constant: 1,000 rows × 27 operations
/// charged without a constant, 26.4 of them per row named, leave 600.
#[test]
fn attributing_to_constants_keeps_the_charge() {
    let per_row = NodeCostTerms::of(CostConstant::HashAggregateOperationsPerRow, 1.0)
        .plus(CostConstant::KmvOperationsPerComparedHash, 20.8);
    let operator = NodeCostTerms::without_constant(27_000.0)
        .plus(CostConstant::OutputRowOperationsPerGroup, 10.0);
    let terms = operator
        .clone()
        .attributing_to_constants(&per_row.times(1_000.0));
    assert_eq!(terms.operations(), operator.operations());
    assert!((terms.operations_without_constant - 600.0).abs() < 1e-9);
    assert_eq!(
        terms.units_per_constant[&CostConstant::HashAggregateOperationsPerRow],
        1_000.0
    );
}
