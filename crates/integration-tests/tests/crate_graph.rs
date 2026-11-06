//! The workspace crate graph follows #1054's target graph (its rule 4).
//!
//! The test reads `cargo metadata --format-version 1 --no-deps` and checks every
//! `[dependencies]` edge between two workspace crates, and every
//! `[dev-dependencies]` edge between two stage crates (a test of one stage
//! never calls another stage). Build-dependencies are not checked. Each crate
//! has a role ([`role_of`]), and [`rule_violated`] says which role may depend
//! on which:
//!
//! - **stage crates** (the frontends, Stage 0's memo, Stage 1's Pass 1 and
//!   Pass 2, Stage 2's physical optimization, Stage 3's plan selection) never
//!   depend on each other; they may depend on the shared crates;
//! - **asap-accuracy** and **asap-cost** depend only on asap-types;
//! - **asap-types** depends on no workspace crate;
//! - the **facade** (asap-planner) may depend on every stage crate and every
//!   shared crate;
//! - the **executor** is a runtime, not a stage; it depends only on asap-types
//!   (#1016 decides where it belongs);
//! - **devtools** and **integration-tests** may depend on anything, and no
//!   other crate may depend on them.
//!
//! The `[dependencies]` edges the rules forbid but the workspace has today are
//! listed, each with its reason, in [`EXCEPTIONS`]; no `[dev-dependencies]`
//! edge between stage crates is excepted. A new workspace crate fails the test
//! until it gets a role.

use serde_json::Value;

/// What a workspace crate is, for the dependency rules.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Role {
    /// `asap-types`: the shared IR and stage-boundary types.
    Types,
    /// `asap-accuracy`, `asap-cost`: the shared accuracy and cost models.
    SharedModel,
    /// Helper crates of the frontends that are no stage themselves.
    FrontendHelper,
    /// A stage crate: a frontend, Stage 0 (memo), Stage 1 (Pass 1 or Pass 2),
    /// Stage 2 (physical optimization) or Stage 3 (plan selection).
    Stage,
    /// `asap-planner`: orchestrates the stages and owns the public API.
    Facade,
    /// `asap-executor`: the Planner's own runtime.
    Runtime,
    /// `asap-devtools`, `asap-integration-tests`: no restriction.
    Unrestricted,
}

/// The role of workspace crate `name`; `None` for a crate not classified yet.
fn role_of(name: &str) -> Option<Role> {
    Some(match name {
        "asap-types" => Role::Types,
        "asap-accuracy" | "asap-cost" => Role::SharedModel,
        "asap-frontend-common" | "asap-sql-function-catalog" | "metricsql_common" => {
            Role::FrontendHelper
        }
        "asap-frontend-promql"
        | "asap-frontend-sql"
        | "asap-frontend-metricsql"
        | "asap-stage0-memo"
        | "asap-stage1-pass1-per-computation-candidates"
        | "asap-stage1-pass2-reuse-candidates"
        | "asap-stage2-physical-optimization"
        | "asap-stage3-plan-selection" => Role::Stage,
        "asap-planner" => Role::Facade,
        "asap-executor" => Role::Runtime,
        "asap-devtools" | "asap-integration-tests" => Role::Unrestricted,
        _ => return None,
    })
}

/// The rule a `[dependencies]` edge from a `from` crate to a `to` crate
/// breaks, or `None` when the edge is allowed.
fn rule_violated(from: Role, to: Role) -> Option<&'static str> {
    use Role::*;
    match (from, to) {
        (Unrestricted, _) => None,
        (_, Unrestricted) => Some("no crate depends on devtools or integration-tests"),
        (Types, _) => Some("asap-types depends on no workspace crate"),
        (SharedModel, Types) => None,
        (SharedModel, _) => Some("asap-accuracy and asap-cost depend only on asap-types"),
        (FrontendHelper, Types | FrontendHelper) => None,
        (FrontendHelper, _) => {
            Some("a frontend helper crate depends only on asap-types and other helpers")
        }
        (Stage, Stage) => Some("stage crates never depend on each other"),
        (Stage, Types | SharedModel | FrontendHelper) => None,
        (Stage, _) => Some("a stage crate depends only on the shared crates"),
        (Facade, Types | SharedModel | FrontendHelper | Stage) => None,
        (Facade, _) => Some("the facade depends only on the stage crates and the shared crates"),
        (Runtime, Types) => None,
        (Runtime, _) => Some("the executor depends only on asap-types"),
    }
}

/// A forbidden edge the workspace keeps on purpose: (from, to).
type Exception = (&'static str, &'static str);

/// The forbidden `[dependencies]` edges the workspace keeps today.
const EXCEPTIONS: &[Exception] = &[
    // `plan_installable` offers only the summaries the Planner's own executor
    // implements. #1016 decides where that kernel list and the executor
    // belong; this exception goes away with it.
    ("asap-planner", "asap-executor"),
];

/// One message per edge of `metadata` (the JSON of
/// `cargo metadata --format-version 1 --no-deps`) that breaks a rule, naming
/// the edge and the rule: a `[dependencies]` edge that [`rule_violated`]
/// forbids, or a `[dev-dependencies]` edge between two stage crates.
fn crate_graph_violations(metadata: &Value) -> Vec<String> {
    let packages = metadata["packages"]
        .as_array()
        .expect("cargo metadata lists `packages`");
    let names: Vec<&str> = packages
        .iter()
        .map(|p| p["name"].as_str().expect("package `name`"))
        .collect();
    let mut violations = Vec::new();
    for package in packages {
        let from = package["name"].as_str().expect("package `name`");
        let Some(from_role) = role_of(from) else {
            violations.push(format!(
                "workspace crate `{from}` has no role: add it to `role_of` in \
                 crates/integration-tests/tests/crate_graph.rs"
            ));
            continue;
        };
        for dependency in package["dependencies"].as_array().into_iter().flatten() {
            let to = dependency["name"].as_str().expect("dependency `name`");
            if !names.contains(&to) || to == from {
                continue;
            }
            let Some(to_role) = role_of(to) else {
                // Reported once, as its own package above.
                continue;
            };
            // `kind` is null for `[dependencies]`, "dev" or "build" otherwise.
            let (section, rule, exceptions) = match dependency["kind"].as_str() {
                None => (
                    "[dependencies]",
                    rule_violated(from_role, to_role),
                    EXCEPTIONS,
                ),
                Some("dev") => (
                    "[dev-dependencies]",
                    (from_role == Role::Stage && to_role == Role::Stage)
                        .then_some("a stage crate's tests never call another stage crate"),
                    &[][..],
                ),
                Some(_) => continue,
            };
            let Some(rule) = rule else {
                continue;
            };
            if !exceptions.contains(&(from, to)) {
                violations.push(format!(
                    "forbidden {section} edge {from} -> {to}: {rule} (#1054)"
                ));
            }
        }
    }
    violations
}

#[test]
fn workspace_crate_graph_follows_the_target_graph() {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let manifest = concat!(env!("CARGO_MANIFEST_DIR"), "/../../Cargo.toml");
    let output = std::process::Command::new(cargo)
        .args([
            "metadata",
            "--format-version",
            "1",
            "--no-deps",
            "--manifest-path",
        ])
        .arg(manifest)
        .output()
        .expect("cargo metadata runs");
    assert!(
        output.status.success(),
        "cargo metadata failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let metadata: Value = serde_json::from_slice(&output.stdout).expect("cargo metadata is JSON");
    let violations = crate_graph_violations(&metadata);
    assert!(violations.is_empty(), "{}", violations.join("\n"));
}

/// A package of synthetic `cargo metadata` JSON with `[dependencies]` on
/// `deps` and `[dev-dependencies]` on `dev_deps`.
fn package(name: &str, deps: &[&str], dev_deps: &[&str]) -> Value {
    let normal = deps
        .iter()
        .map(|d| serde_json::json!({"name": d, "kind": null}));
    let dev = dev_deps
        .iter()
        .map(|d| serde_json::json!({"name": d, "kind": "dev"}));
    serde_json::json!({"name": name, "dependencies": normal.chain(dev).collect::<Vec<_>>()})
}

#[test]
fn checker_rejects_an_edge_between_stage_crates() {
    let metadata = serde_json::json!({"packages": [
        package("asap-types", &[], &[]),
        package("asap-cost", &["asap-types"], &[]),
        package("asap-stage2-physical-optimization", &["asap-types"], &[]),
        // Forbidden: Stage 3 on Stage 2.
        package(
            "asap-stage3-plan-selection",
            &["asap-cost", "asap-stage2-physical-optimization"],
            &[],
        ),
    ]});
    assert_eq!(
        crate_graph_violations(&metadata),
        vec![
            "forbidden [dependencies] edge asap-stage3-plan-selection -> \
             asap-stage2-physical-optimization: stage crates never depend on each other (#1054)"
                .to_string()
        ]
    );
}

#[test]
fn checker_ignores_external_crates_and_non_stage_dev_dependencies() {
    let metadata = serde_json::json!({"packages": [
        package("asap-types", &["serde"], &[]),
        package("asap-stage1-pass1-per-computation-candidates", &["asap-types"], &[]),
        package("asap-executor", &["asap-types"], &["asap-stage1-pass1-per-computation-candidates"]),
    ]});
    assert!(crate_graph_violations(&metadata).is_empty());
}

#[test]
fn checker_rejects_a_dev_dependency_between_stage_crates() {
    let metadata = serde_json::json!({"packages": [
        package("asap-types", &[], &[]),
        package("asap-stage0-memo", &["asap-types"], &[]),
        // Forbidden: Pass 1's tests call Stage 0.
        package(
            "asap-stage1-pass1-per-computation-candidates",
            &["asap-types"],
            &["asap-stage0-memo"],
        ),
    ]});
    assert_eq!(
        crate_graph_violations(&metadata),
        vec![
            "forbidden [dev-dependencies] edge asap-stage1-pass1-per-computation-candidates -> \
             asap-stage0-memo: a stage crate's tests never call another stage crate (#1054)"
                .to_string()
        ]
    );
}

#[test]
fn checker_rejects_an_unclassified_crate_and_shared_model_edges() {
    let metadata = serde_json::json!({"packages": [
        package("asap-types", &[], &[]),
        package("asap-stage1-pass1-per-computation-candidates", &["asap-types"], &[]),
        package("asap-accuracy", &["asap-stage1-pass1-per-computation-candidates"], &[]),
        package("asap-new-crate", &[], &[]),
    ]});
    assert_eq!(
        crate_graph_violations(&metadata),
        vec![
            "forbidden [dependencies] edge asap-accuracy -> asap-stage1-pass1-per-computation-candidates: \
             asap-accuracy and asap-cost depend only on asap-types (#1054)"
                .to_string(),
            "workspace crate `asap-new-crate` has no role: add it to `role_of` in \
             crates/integration-tests/tests/crate_graph.rs"
                .to_string(),
        ]
    );
}
