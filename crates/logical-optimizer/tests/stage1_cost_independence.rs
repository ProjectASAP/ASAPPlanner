//! Stage 1 (logical candidate generation) stays independent of the cost
//! model: only Stage 3 prices plans (#572, decision Q36(a)).

use std::path::{Path, PathBuf};

/// Module paths Stage 1 production code must not name.
const FORBIDDEN_MODULES: &[&str] = &["cost_model", "recurrence"];

/// Crates Stage 1 must not depend on: later stages, the facade and the executor.
const FORBIDDEN_CRATES: &[&str] = &[
    "asap-physical-optimizer",
    "asap-plan-selection",
    "asap-planner",
    "asap-physical-operators",
];

/// Every `.rs` file under `dir`.
fn rust_files(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            files.extend(rust_files(&path));
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            files.push(path);
        }
    }
    files.sort();
    files
}

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

/// Stage 1 production code names neither `cost_model` nor `recurrence`.
#[test]
fn stage1_does_not_import_cost_model_or_recurrence() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut offenders = Vec::new();
    for file in rust_files(&src) {
        let source = std::fs::read_to_string(&file).unwrap();
        for (number, line) in production_lines(&source) {
            if FORBIDDEN_MODULES
                .iter()
                .any(|module| line.contains(&format!("{module}::")))
            {
                let file = file.strip_prefix(&src).unwrap().display();
                offenders.push(format!("{file}:{number}: {}", line.trim()));
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "Stage 1 must not depend on the cost model:\n{}",
        offenders.join("\n")
    );
}

/// The manifest names no later stage, facade or executor crate, so Cargo
/// rejects any import of them.
#[test]
fn stage1_manifest_has_no_path_back_to_later_stages() {
    let manifest =
        std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml")).unwrap();
    let offenders: Vec<&str> = manifest
        .lines()
        .filter(|line| !line.trim_start().starts_with('#'))
        .filter(|line| FORBIDDEN_CRATES.iter().any(|name| line.contains(name)))
        .collect();
    assert!(
        offenders.is_empty(),
        "asap-logical-optimizer must not depend on a later stage:\n{}",
        offenders.join("\n")
    );
}
