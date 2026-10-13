//! Only plan selection prices plans (#509): logical ASAP-aware optimization
//! and physical ASAP-aware optimization import no cost model.

use std::path::Path;

/// The logical and physical ASAP-aware optimization modules, relative to
/// `src/`.
const OPTIMIZATION_MODULES: &[&str] = &[
    "per_computation_candidates.rs",
    "stage2_physical_asap_optimization.rs",
];

/// The modules that price plans.
const COST_MODULES: &[&str] = &[
    "analytical_cost",
    "physical_operator_statistics",
    "stage3_plan_selection",
];

/// The source lines outside `#[cfg(test)]` items and comments, numbered.
fn production_lines(source: &str) -> Vec<(usize, &str)> {
    let mut lines = Vec::new();
    let mut skip_next_item = false;
    let mut depth = 0i64;
    for (index, line) in source.lines().enumerate() {
        let trimmed = line.trim();
        if depth > 0 {
            depth += brace_balance(line);
            continue;
        }
        if trimmed == "#[cfg(test)]" {
            skip_next_item = true;
            continue;
        }
        if skip_next_item {
            if trimmed.starts_with("#[") || trimmed.is_empty() {
                continue;
            }
            skip_next_item = false;
            depth = brace_balance(line);
            continue;
        }
        if !trimmed.starts_with("//") {
            lines.push((index + 1, line));
        }
    }
    lines
}

fn brace_balance(line: &str) -> i64 {
    line.chars()
        .map(|c| match c {
            '{' => 1,
            '}' => -1,
            _ => 0,
        })
        .sum()
}

/// Names `lib.rs` re-exports from `module` (`pub use module::{...};`).
fn reexports<'a>(lib: &'a str, module: &str) -> Vec<&'a str> {
    let start = format!("pub use {module}::{{");
    let Some(begin) = lib.find(&start) else {
        return Vec::new();
    };
    let rest = &lib[begin + start.len()..];
    rest[..rest.find('}').expect("re-export list is closed")]
        .split(',')
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .collect()
}

/// The production lines of `source` that import or name a cost module.
fn cost_imports(source: &str, forbidden: &[&str]) -> Vec<(usize, String)> {
    let mut offenders = Vec::new();
    let mut in_use = false;
    for (number, line) in production_lines(source) {
        in_use |= line.trim_start().starts_with("use ") || line.contains(" use ");
        let path_use = COST_MODULES
            .iter()
            .any(|module| line.contains(&format!("{module}::")));
        let imported = in_use
            && line
                .split(|c: char| !(c.is_alphanumeric() || c == '_'))
                .any(|token| forbidden.contains(&token));
        if path_use || imported {
            offenders.push((number, line.trim().to_string()));
        }
        if line.contains(';') {
            in_use = false;
        }
    }
    offenders
}

#[test]
fn logical_and_physical_asap_optimization_import_no_cost_model() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let lib = std::fs::read_to_string(src.join("lib.rs")).unwrap();
    let forbidden: Vec<&str> = COST_MODULES
        .iter()
        .copied()
        .chain(
            COST_MODULES
                .iter()
                .flat_map(|module| reexports(&lib, module)),
        )
        .collect();
    let mut offenders = Vec::new();
    for module in OPTIMIZATION_MODULES {
        let source = std::fs::read_to_string(src.join(module)).unwrap();
        for (number, line) in cost_imports(&source, &forbidden) {
            offenders.push(format!("{module}:{number}: {line}"));
        }
    }
    assert!(
        offenders.is_empty(),
        "only plan selection may price plans:\n{}",
        offenders.join("\n")
    );
}

/// The check finds a cost import in production code, through a path or a
/// re-exported name, and ignores tests and comments.
#[test]
fn the_check_finds_cost_imports_outside_tests() {
    let source = "\
// use crate::analytical_cost::estimate_operator;
use crate::{
    accuracy::AccuracyModel,
    ResourceCalibration,
};
fn f() -> f64 {
    crate::analytical_cost::price()
}
#[cfg(test)]
mod tests {
    use crate::analytical_cost::estimate_operator;
}
";
    let lines: Vec<_> = cost_imports(source, &["ResourceCalibration"])
        .into_iter()
        .map(|(number, _)| number)
        .collect();
    assert_eq!(lines, vec![4, 7]);
}
