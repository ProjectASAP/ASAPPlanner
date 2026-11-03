//! The workspace crate graph follows #1054's target graph (its rule 4).
//!
//! The test reads `cargo metadata --format-version 1 --no-deps` and checks every
//! `[dependencies]` edge between two workspace crates. Dev- and
//! build-dependencies are not checked. Each crate has a role ([`role_of`]), and
//! [`rule_violated`] says which role may depend on which:
//!
//! - **stage crates** (the frontends, logical, physical, plan-selection) never
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
//! The edges the rules forbid but the workspace has today are listed, each
//! with its reason, in [`EXCEPTIONS`]. A new workspace crate fails the test
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
    /// A stage crate: a frontend (Stage 0), logical (Stage 1), physical
    /// (Stage 2) or plan-selection (Stage 3).
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
        | "asap-logical-asap-optimization"
        | "asap-physical-asap-optimization"
        | "asap-plan-selection" => Role::Stage,
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

/// A forbidden edge the workspace keeps on purpose.
struct Exception {
    from: &'static str,
    to: &'static str,
    /// The exception holds only while the dependency is `optional = true`.
    only_if_optional: bool,
}

/// The forbidden edges the workspace keeps today.
const EXCEPTIONS: &[Exception] = &[
    // Logical's `test-support` feature lowers PromQL test queries for other
    // crates' tests. The dependency is optional and enabled only by that
    // feature, so no production build links Stage 0 into Stage 1. Made
    // non-optional, the edge fails the test.
    Exception {
        from: "asap-logical-asap-optimization",
        to: "asap-frontend-promql",
        only_if_optional: true,
    },
    // `plan_installable` offers only the summaries the Planner's own executor
    // implements. #1016 decides where that kernel list and the executor
    // belong; this exception goes away with it.
    Exception {
        from: "asap-planner",
        to: "asap-executor",
        only_if_optional: false,
    },
];

/// One message per `[dependencies]` edge of `metadata` (the JSON of
/// `cargo metadata --format-version 1 --no-deps`) that breaks a rule, naming
/// the edge and the rule.
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
            // `kind` is null for `[dependencies]`, "dev" or "build" otherwise.
            if !dependency["kind"].is_null() {
                continue;
            }
            let to = dependency["name"].as_str().expect("dependency `name`");
            if !names.contains(&to) {
                continue;
            }
            let Some(to_role) = role_of(to) else {
                // Reported once, as its own package above.
                continue;
            };
            let Some(rule) = rule_violated(from_role, to_role) else {
                continue;
            };
            let optional = dependency["optional"].as_bool().unwrap_or(false);
            let excepted = EXCEPTIONS
                .iter()
                .any(|e| e.from == from && e.to == to && (optional || !e.only_if_optional));
            if !excepted {
                violations.push(format!(
                    "forbidden [dependencies] edge {from} -> {to}: {rule} (#1054)"
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
/// `deps` (name, optional) and a dev-dependency on `dev_deps`.
fn package(name: &str, deps: &[(&str, bool)], dev_deps: &[&str]) -> Value {
    let normal = deps
        .iter()
        .map(|(d, optional)| serde_json::json!({"name": d, "kind": null, "optional": optional}));
    let dev = dev_deps
        .iter()
        .map(|d| serde_json::json!({"name": d, "kind": "dev", "optional": false}));
    serde_json::json!({"name": name, "dependencies": normal.chain(dev).collect::<Vec<_>>()})
}

#[test]
fn checker_rejects_an_edge_between_stage_crates() {
    let metadata = serde_json::json!({"packages": [
        package("asap-types", &[], &[]),
        package("asap-cost", &[("asap-types", false)], &[]),
        package("asap-physical-asap-optimization", &[("asap-types", false)], &[]),
        // Forbidden: Stage 3 on Stage 2.
        package(
            "asap-plan-selection",
            &[("asap-cost", false), ("asap-physical-asap-optimization", false)],
            &[],
        ),
    ]});
    assert_eq!(
        crate_graph_violations(&metadata),
        vec!["forbidden [dependencies] edge asap-plan-selection -> \
             asap-physical-asap-optimization: stage crates never depend on each other (#1054)"
            .to_string()]
    );
}

#[test]
fn checker_ignores_dev_dependencies_and_external_crates() {
    let metadata = serde_json::json!({"packages": [
        package("asap-types", &[("serde", false)], &[]),
        package("asap-logical-asap-optimization", &[("asap-types", false)], &[]),
        package(
            "asap-plan-selection",
            &[("asap-types", false)],
            &["asap-logical-asap-optimization"],
        ),
    ]});
    assert!(crate_graph_violations(&metadata).is_empty());
}

#[test]
fn optional_only_exception_rejects_the_edge_made_non_optional() {
    let metadata = |optional| {
        serde_json::json!({"packages": [
            package("asap-types", &[], &[]),
            package("asap-frontend-promql", &[("asap-types", false)], &[]),
            package(
                "asap-logical-asap-optimization",
                &[("asap-types", false), ("asap-frontend-promql", optional)],
                &[],
            ),
        ]})
    };
    assert!(crate_graph_violations(&metadata(true)).is_empty());
    assert_eq!(
        crate_graph_violations(&metadata(false)),
        vec![
            "forbidden [dependencies] edge asap-logical-asap-optimization -> \
             asap-frontend-promql: stage crates never depend on each other (#1054)"
                .to_string()
        ]
    );
}

#[test]
fn checker_rejects_an_unclassified_crate_and_shared_model_edges() {
    let metadata = serde_json::json!({"packages": [
        package("asap-types", &[], &[]),
        package("asap-logical-asap-optimization", &[("asap-types", false)], &[]),
        package("asap-accuracy", &[("asap-logical-asap-optimization", false)], &[]),
        package("asap-new-crate", &[], &[]),
    ]});
    assert_eq!(
        crate_graph_violations(&metadata),
        vec![
            "forbidden [dependencies] edge asap-accuracy -> asap-logical-asap-optimization: \
             asap-accuracy and asap-cost depend only on asap-types (#1054)"
                .to_string(),
            "workspace crate `asap-new-crate` has no role: add it to `role_of` in \
             crates/integration-tests/tests/crate_graph.rs"
                .to_string(),
        ]
    );
}
