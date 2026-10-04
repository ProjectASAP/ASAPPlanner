//! Stage 3 (plan selection) depends on neither the facade nor the executor: Cargo enforces
//! the one-way #509 stage flow (#572).

use std::path::Path;

/// Crates Stage 3 must not depend on: the facade and the executor.
const FORBIDDEN_CRATES: &[&str] = &[
    "asap-aware-mapping",
    "asap-planner",
    "asap-physical-operators",
];

/// The manifest names neither the facade nor the executor crate, so Cargo
/// rejects any import of them.
#[test]
fn stage3_manifest_has_no_path_to_the_facade_or_executor() {
    let manifest =
        std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml")).unwrap();
    let offenders: Vec<&str> = manifest
        .lines()
        .filter(|line| !line.trim_start().starts_with('#'))
        .filter(|line| FORBIDDEN_CRATES.iter().any(|name| line.contains(name)))
        .collect();
    assert!(
        offenders.is_empty(),
        "asap-plan-selection must not depend on the facade or the executor:\n{}",
        offenders.join("\n")
    );
}
