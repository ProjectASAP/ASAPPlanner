//! Stage 2 (physical candidates) depends on no later stage: Cargo enforces
//! the one-way #509 stage flow (#572).

use std::path::Path;

/// Crates Stage 2 must not depend on: Stage 3, the facade and the executor.
const FORBIDDEN_CRATES: &[&str] = &[
    "asap-plan-selection",
    "asap-planner",
    "asap-physical-operators",
];

/// The manifest names no later stage, facade or executor crate, so Cargo
/// rejects any import of them.
#[test]
fn stage2_manifest_has_no_path_back_to_later_stages() {
    let manifest =
        std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml")).unwrap();
    let offenders: Vec<&str> = manifest
        .lines()
        .filter(|line| !line.trim_start().starts_with('#'))
        .filter(|line| FORBIDDEN_CRATES.iter().any(|name| line.contains(name)))
        .collect();
    assert!(
        offenders.is_empty(),
        "asap-physical-optimizer must not depend on a later stage:\n{}",
        offenders.join("\n")
    );
}
