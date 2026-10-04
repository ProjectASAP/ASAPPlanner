//! Stage 1 (logical candidate generation) stays independent of the cost
//! model: only Stage 3 prices plans (#572, decision Q36(a)).

use std::path::Path;

/// The Stage 1 modules, relative to `src/`.
const STAGE1_MODULES: &[&str] = &[
    "replacement.rs",
    "exact_composition.rs",
    "grouping.rs",
    "rollup.rs",
    "topk_reuse.rs",
    "rewrite.rs",
    "maintained_population.rs",
    "accuracy/reconciliation.rs",
    "logical_candidates.rs",
    "function_rules.rs",
];

const FORBIDDEN_MODULES: &[&str] = &["cost_model", "recurrence"];

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

/// Stage 1 production code imports neither `cost_model` nor `recurrence`.
#[test]
fn stage1_does_not_import_cost_model_or_recurrence() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let lib = std::fs::read_to_string(src.join("lib.rs")).unwrap();
    let forbidden: Vec<&str> = FORBIDDEN_MODULES
        .iter()
        .copied()
        .chain(
            FORBIDDEN_MODULES
                .iter()
                .flat_map(|module| reexports(&lib, module)),
        )
        .collect();
    let mut offenders = Vec::new();
    for module in STAGE1_MODULES {
        let source = std::fs::read_to_string(src.join(module)).unwrap();
        let mut in_use = false;
        for (number, line) in production_lines(&source) {
            in_use |= line.trim_start().starts_with("use ") || line.contains(" use ");
            let path_use = FORBIDDEN_MODULES
                .iter()
                .any(|module| line.contains(&format!("{module}::")));
            let imported = in_use
                && line
                    .split(|c: char| !(c.is_alphanumeric() || c == '_'))
                    .any(|token| forbidden.contains(&token));
            if path_use || imported {
                offenders.push(format!("{module}:{number}: {}", line.trim()));
            }
            if line.contains(';') {
                in_use = false;
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "Stage 1 must not depend on the cost model:\n{}",
        offenders.join("\n")
    );
}
