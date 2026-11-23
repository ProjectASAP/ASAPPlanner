//! The golden runner: one test per case directory under `tests/goldens/`
//! (design doc `testing-infrastructure`, D5–D8, D15, D25).
//!
//! A case directory holds a `case.toml` (spec in the design doc), its
//! workload files, optional `cost_inputs.json`, `deployment_capabilities.json`
//! and `tables/*.json`, and `expected.json`, the `asap_planner --format json`
//! output. The runner plans the case with the `asap_planner` binary and
//! compares its output with `expected.json`: floats within a relative
//! tolerance of [`RELATIVE_TOLERANCE`], everything else exactly. A failure
//! prints a line diff and the command that reproduces it.
//!
//! `tests/goldens/defaults/` holds the inputs a case does not give: a case
//! file replaces the default file, and a workload without `data_workload`
//! gets `defaults/data_workload.json`.
//!
//! Run a subset by name: `cargo test --release -p asap-devtools --test
//! goldens stage0/`. Regenerate `expected.json` with `UPDATE_SNAPSHOTS=1`:
//! a case's file is rewritten only when the output no longer matches it,
//! and the case's assertions still run, so a wrong output is not written.
//! `tests/goldens/runner/` is reserved for the runner's own checks.

use std::path::{Path, PathBuf};
use std::process::Command;

use libtest_mimic::{Arguments, Failed, Trial};
use serde::Deserialize;
use serde_json::Value;

/// Floats in the output match `expected.json` within this relative
/// difference: about one unit in the 5th significant digit, the printer's
/// precision (D2), so a value near a rounding boundary does not flip.
const RELATIVE_TOLERANCE: f64 = 1e-4;

/// `case.toml` fields reserved by the design doc whose support has not
/// landed, with the PR of #1354 that adds each.
const NOT_YET_SUPPORTED: &[(&str, &str)] = &[
    ("rule_fires", "PR 5"),
    ("rule_does_not_fire", "PR 5"),
    ("mutations", "PR 6"),
    // No workflow runs a nightly tier yet, so such a case would never run.
    ("tier", "a nightly golden workflow"),
];

/// A parsed `case.toml`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Case {
    why: String,
    #[serde(default = "default_args")]
    args: Vec<String>,
    #[serde(default = "default_workloads")]
    workloads: Vec<String>,
    #[serde(default)]
    same_group: Vec<[String; 2]>,
    #[serde(default)]
    different_group: Vec<[String; 2]>,
}

fn default_args() -> Vec<String> {
    vec!["--run-until".into(), "stage3".into()]
}

fn default_workloads() -> Vec<String> {
    vec!["workload.json".into()]
}

fn parse_case(text: &str) -> Result<Case, String> {
    let table: toml::Table = toml::from_str(text).map_err(|e| e.to_string())?;
    for (field, until) in NOT_YET_SUPPORTED {
        if table.contains_key(*field) {
            return Err(format!("`{field}` is not supported until {until}"));
        }
    }
    let case: Case = toml::from_str(text).map_err(|e| e.to_string())?;
    if case.why.trim().is_empty() {
        return Err("`why` is empty".into());
    }
    Ok(case)
}

fn goldens_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/goldens")
}

fn defaults_dir() -> PathBuf {
    goldens_dir().join("defaults")
}

/// This process's scratch directory: merged workloads, planner outputs
/// and the runner checks' cases. Per process, so concurrent runs (e.g.
/// debug and release, which share `CARGO_TARGET_TMPDIR`) do not overwrite
/// each other's files; removed when every trial passes.
fn scratch_root() -> PathBuf {
    PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("goldens-{}", std::process::id()))
}

/// Every case directory under `root` (one holding a `case.toml`), with its
/// name relative to `root`, sorted. `root/defaults/` holds no cases;
/// `root/runner/` is refused, since the runner's checks use those names.
fn case_dirs(root: &Path) -> Result<Vec<(String, PathBuf)>, String> {
    if root.join("runner").exists() {
        return Err(format!(
            "{}: reserved for the runner's own checks; move these cases",
            root.join("runner").display()
        ));
    }
    let mut found = Vec::new();
    let mut pending = vec![root.to_owned()];
    while let Some(dir) = pending.pop() {
        if dir.join("case.toml").is_file() {
            let name = dir.strip_prefix(root).unwrap();
            found.push((name.to_string_lossy().replace('\\', "/"), dir));
            continue;
        }
        for entry in std::fs::read_dir(&dir).map_err(|e| format!("{}: {e}", dir.display()))? {
            let path = entry.map_err(|e| e.to_string())?.path();
            if path.is_dir() && path != root.join("defaults") {
                pending.push(path);
            }
        }
    }
    found.sort();
    Ok(found)
}

fn main() {
    let arguments = Arguments::from_args();
    let update = std::env::var_os("UPDATE_SNAPSHOTS").is_some_and(|value| value == "1");
    let mut trials: Vec<Trial> = match case_dirs(&goldens_dir()) {
        Ok(cases) => cases
            .into_iter()
            .map(|(name, dir)| {
                Trial::test(name.clone(), move || {
                    run_case(&name, &dir, update).map_err(Failed::from)
                })
            })
            .collect(),
        Err(error) => vec![Trial::test("runner/case_discovery", move || {
            Err(error.into())
        })],
    };
    trials.extend(runner_checks());
    let conclusion = libtest_mimic::run(&arguments, trials);
    // A failure's files stay, so its printed command still reproduces it.
    if !conclusion.has_failed() {
        let _ = std::fs::remove_dir_all(scratch_root());
    }
    conclusion.exit();
}

/// Plans the case in `dir` and checks its assertions, then compares the
/// output with `expected.json`, or writes it there when `update`.
fn run_case(name: &str, dir: &Path, update: bool) -> Result<(), String> {
    let text = std::fs::read_to_string(dir.join("case.toml")).map_err(|e| e.to_string())?;
    let case = parse_case(&text).map_err(|e| format!("{name}/case.toml: {e}"))?;
    let scratch = scratch_root().join("cases").join(name);
    std::fs::create_dir_all(&scratch).map_err(|e| e.to_string())?;
    let command = planner_command(&case, dir, &scratch)?;
    let reproduce = format!(
        "reproduce: {}\nregenerate: UPDATE_SNAPSHOTS=1 cargo test --release -p asap-devtools \
         --test goldens -- --exact {name}",
        shell_line(&command)
    );
    let actual = run_planner(&command, &scratch.join("actual.json"))
        .map_err(|e| format!("{e}\n{reproduce}"))?;
    check_assertions(&case, &actual).map_err(|e| format!("{e}\n{reproduce}"))?;
    let actual_text = serde_json::to_string_pretty(&actual).unwrap() + "\n";
    let expected_path = dir.join("expected.json");
    if update {
        // Rewriting a file the output still matches would turn
        // sub-tolerance float noise into diffs.
        let matches =
            read_json(&expected_path).is_ok_and(|expected| json_matches(&expected, &actual));
        if matches {
            return Ok(());
        }
        return std::fs::write(&expected_path, actual_text).map_err(|e| e.to_string());
    }
    let expected_text = std::fs::read_to_string(&expected_path).map_err(|e| {
        format!(
            "{}: {e}; write it with UPDATE_SNAPSHOTS=1\n{reproduce}",
            expected_path.display()
        )
    })?;
    let expected: Value = serde_json::from_str(&expected_text)
        .map_err(|e| format!("{}: {e}", expected_path.display()))?;
    if json_matches(&expected, &actual) {
        return Ok(());
    }
    Err(format!(
        "output differs from expected.json (floats within relative {RELATIVE_TOLERANCE}):\n{}\n{reproduce}",
        line_diff(&expected_text, &actual_text)
    ))
}

/// The `asap_planner` command line for `case`, with absolute paths: the
/// case's `args`, then its inputs, each from the case or the defaults.
fn planner_command(case: &Case, dir: &Path, scratch: &Path) -> Result<Vec<String>, String> {
    let mut command = vec![env!("CARGO_BIN_EXE_asap_planner").to_owned()];
    command.extend(case.args.iter().cloned());
    command.extend(["--format".into(), "json".into()]);
    for (index, file) in case.workloads.iter().enumerate() {
        command.push("--workload".into());
        command.push(workload_path(index, &dir.join(file), scratch)?);
    }
    for (file, flag) in [
        ("cost_inputs.json", "--cost-inputs"),
        ("deployment_capabilities.json", "--deployment-capabilities"),
    ] {
        let path = [dir.join(file), defaults_dir().join(file)]
            .into_iter()
            .find(|path| path.is_file());
        if let Some(path) = path {
            command.push(flag.into());
            command.push(path.display().to_string());
        }
    }
    let tables = dir.join("tables");
    if tables.is_dir() {
        let mut paths: Vec<PathBuf> = std::fs::read_dir(&tables)
            .map_err(|e| e.to_string())?
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
            .collect();
        paths.sort();
        for path in paths {
            // The flag takes the table's JSON, not a path.
            let table: Value = read_json(&path)?;
            command.push("--sql-catalog-table".into());
            command.push(table.to_string());
        }
    }
    Ok(command)
}

/// `path`, or, when its workload has no `data_workload`, a copy in
/// `scratch` with `defaults/data_workload.json`, prefixed by its `index`
/// in `workloads` so files of the same name do not collide. The copy
/// outlives the test, so the printed command reproduces the failure.
fn workload_path(index: usize, path: &Path, scratch: &Path) -> Result<String, String> {
    let mut workload: Value = read_json(path)?;
    if !workload["data_workload"].is_null() {
        return Ok(path.display().to_string());
    }
    workload["data_workload"] = read_json(&defaults_dir().join("data_workload.json"))?;
    let name = path.file_name().unwrap().to_string_lossy();
    let copy = scratch.join(format!("{index}-{name}"));
    std::fs::write(&copy, serde_json::to_string_pretty(&workload).unwrap())
        .map_err(|e| e.to_string())?;
    Ok(copy.display().to_string())
}

fn read_json(path: &Path) -> Result<Value, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))
}

/// Runs `command` with `--out out` and returns the document it wrote. A
/// run that failed after writing its document (a planning error the
/// document records) still returns it; a run that wrote none, or whose
/// exit status disagrees with the document ([`exit_status_agrees`]), fails.
fn run_planner(command: &[String], out: &Path) -> Result<Value, String> {
    let _ = std::fs::remove_file(out);
    let result = Command::new(&command[0])
        .args(&command[1..])
        .arg("--out")
        .arg(out)
        .output()
        .map_err(|e| format!("{}: {e}", command[0]))?;
    let stderr = String::from_utf8_lossy(&result.stderr);
    let text = std::fs::read_to_string(out).map_err(|_| {
        format!(
            "asap_planner wrote no output ({}):\n{stderr}",
            result.status
        )
    })?;
    let document: Value =
        serde_json::from_str(&text).map_err(|e| format!("{}: {e}", out.display()))?;
    exit_status_agrees(result.status.code(), &document).map_err(|e| format!("{e}\n{stderr}"))?;
    Ok(document)
}

/// Whether `asap_planner`'s exit status `code` agrees with its document:
/// 1 exactly when a stage section records an `error` (a planning error the
/// golden records), 0 otherwise. Any other status, or a disagreement, is a
/// failure no golden may record, even under `UPDATE_SNAPSHOTS=1`.
fn exit_status_agrees(code: Option<i32>, document: &Value) -> Result<(), String> {
    let recorded_error = [
        "stage1_logical_asap_optimization",
        "stage2_physical_asap_optimization",
        "stage3_plan_selection",
    ]
    .iter()
    .any(|stage| document[stage]["error"].is_string());
    match (code, recorded_error) {
        (Some(0), false) | (Some(1), true) => Ok(()),
        (code, recorded_error) => Err(format!(
            "asap_planner exited with {code:?}, but its document {} a stage error",
            if recorded_error {
                "records"
            } else {
                "records no"
            }
        )),
    }
}

/// Checks `same_group` and `different_group` against the root groups of
/// the queries in the output's Stage 0 memo.
fn check_assertions(case: &Case, document: &Value) -> Result<(), String> {
    let checks = case
        .same_group
        .iter()
        .map(|pair| (pair, true))
        .chain(case.different_group.iter().map(|pair| (pair, false)));
    for ([a, b], same) in checks {
        let (group_a, group_b) = (root_group(document, a)?, root_group(document, b)?);
        if (group_a == group_b) != same {
            let relation = if same {
                "same_group"
            } else {
                "different_group"
            };
            return Err(format!(
                "{relation} [{a}, {b}] fails: {a} is rooted at group {group_a}, {b} at group {group_b}"
            ));
        }
    }
    Ok(())
}

/// The canonical id of the Stage 0 memo group query `query` (`q1..qN`)
/// is rooted at.
fn root_group(document: &Value, query: &str) -> Result<u64, String> {
    let count = document["workload"]["queries"]
        .as_array()
        .map_or(0, Vec::len);
    let index = query
        .strip_prefix('q')
        .and_then(|n| n.parse::<usize>().ok())
        .filter(|n| (1..=count).contains(n))
        .ok_or_else(|| format!("{query} names no query (the workload has q1..q{count})"))?
        - 1;
    let stage0 = document["memo"]["steps"]
        .as_array()
        .and_then(|steps| steps.iter().find(|step| step["step"] == "stage0"))
        .ok_or("the output has no stage0 memo step")?;
    let roots: Vec<&Value> = stage0["groups"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|group| {
            group["roots_of"]
                .as_array()
                .is_some_and(|roots| roots.iter().any(|root| root.as_u64() == Some(index as u64)))
        })
        .collect();
    match roots[..] {
        [group] => group["id"]
            .as_u64()
            .ok_or_else(|| format!("the root group of {query} has no id")),
        _ => Err(format!(
            "{query} is the root of {} stage0 groups, not one",
            roots.len()
        )),
    }
}

/// Whether `actual` equals `expected`: floats within [`RELATIVE_TOLERANCE`],
/// integers (ids, intervals, counts) and everything else exactly.
fn json_matches(expected: &Value, actual: &Value) -> bool {
    match (expected, actual) {
        (Value::Number(e), Value::Number(a)) if !e.is_f64() && !a.is_f64() => e == a,
        (Value::Number(e), Value::Number(a)) => {
            let (e, a) = (e.as_f64().unwrap(), a.as_f64().unwrap());
            e == a || (e - a).abs() <= RELATIVE_TOLERANCE * e.abs().max(a.abs())
        }
        (Value::Array(e), Value::Array(a)) => {
            e.len() == a.len() && e.iter().zip(a).all(|(e, a)| json_matches(e, a))
        }
        (Value::Object(e), Value::Object(a)) => {
            e.len() == a.len()
                && e.iter()
                    .all(|(key, e)| a.get(key).is_some_and(|a| json_matches(e, a)))
        }
        _ => expected == actual,
    }
}

/// A unified line diff, at most [`MAX_DIFF_LINES`] lines.
fn line_diff(expected: &str, actual: &str) -> String {
    const MAX_DIFF_LINES: usize = 200;
    let diff = similar::TextDiff::from_lines(expected, actual)
        .unified_diff()
        .context_radius(3)
        .header("expected.json", "actual")
        .to_string();
    let lines: Vec<&str> = diff.lines().collect();
    if lines.len() <= MAX_DIFF_LINES {
        return diff;
    }
    format!(
        "{}\n... {} more diff lines",
        lines[..MAX_DIFF_LINES].join("\n"),
        lines.len() - MAX_DIFF_LINES
    )
}

/// `command` as one line a POSIX shell runs unchanged.
fn shell_line(command: &[String]) -> String {
    let quote = |word: &String| {
        let safe = |c: char| c.is_ascii_alphanumeric() || "-_./=:,+".contains(c);
        if !word.is_empty() && word.chars().all(safe) {
            word.clone()
        } else {
            format!("'{}'", word.replace('\'', r"'\''"))
        }
    };
    command.iter().map(quote).collect::<Vec<_>>().join(" ")
}

/// Checks of the runner itself, listed under `runner/`.
fn runner_checks() -> Vec<Trial> {
    vec![
        // Floats match within one unit in the 5th significant digit;
        // integers and other values match exactly.
        Trial::test("runner/numbers_match_within_tolerance", || {
            let matches = |e: &str, a: &str| {
                json_matches(
                    &serde_json::from_str(e).unwrap(),
                    &serde_json::from_str(a).unwrap(),
                )
            };
            let ok = matches("[1.0, 0.0085]", "[1.00001, 0.00850003]")
                && !matches("1.0", "1.001")
                && !matches("0", "1e-9")
                && !matches("60000", "60005")
                && !matches(r#"{"a": "x"}"#, r#"{"a": "y"}"#)
                && !matches(r#"{"a": 1}"#, r#"{"a": 1, "b": 1}"#);
            ok.then_some(())
                .ok_or_else(|| "tolerance check failed".into())
        }),
        // A case without `why`, with an unknown field, or with a field whose
        // support has not landed fails to load, naming the problem.
        Trial::test("runner/rejects_malformed_case_toml", || {
            for (text, expected) in [
                ("args = []", "why"),
                ("why = ' '", "why"),
                ("why = 'x'\nbogus = 1", "bogus"),
                ("why = 'x'\nrule_fires = ['rollup']", "PR 5"),
                ("why = 'x'\nmutations = []", "PR 6"),
                ("why = 'x'\ntier = 'nightly'", "tier"),
            ] {
                match parse_case(text) {
                    Err(error) if error.contains(expected) => {}
                    other => {
                        return Err(format!(
                            "{text:?}: expected an error naming {expected:?}, got {other:?}"
                        )
                        .into())
                    }
                }
            }
            Ok(())
        }),
        // A case whose expected.json is wrong fails with a line diff showing
        // the wrong and the actual value, and the command to reproduce it;
        // the same case with the right expected.json passes.
        Trial::test("runner/wrong_expected_fails_with_diff", || {
            let source = goldens_dir().join("stage0/promql_quantile_over_time");
            let dir = scratch_root().join("checks/wrong_expected");
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir)?;
            for file in ["case.toml", "workload.json"] {
                std::fs::copy(source.join(file), dir.join(file))?;
            }
            // Its own expected.json, not the source's, which an
            // `UPDATE_SNAPSHOTS=1` run may be rewriting concurrently.
            run_case("runner/wrong_expected", &dir, true)?;
            run_case("runner/wrong_expected", &dir, false)?;
            let mut expected = read_json(&dir.join("expected.json"))?;
            expected["workload"]["queries"][0]["text"] = "deliberately wrong".into();
            std::fs::write(
                dir.join("expected.json"),
                serde_json::to_string_pretty(&expected)? + "\n",
            )?;
            let error = run_case("runner/wrong_expected", &dir, false)
                .err()
                .ok_or("a wrong expected.json passed")?;
            let shows = |needle: &str| error.lines().any(|line| line.contains(needle));
            if shows("-  ") && shows("deliberately wrong") && shows("+  ") && shows("reproduce: ") {
                Ok(())
            } else {
                Err(format!("unreadable failure:\n{error}").into())
            }
        }),
        // `UPDATE_SNAPSHOTS=1` leaves an expected.json that already matches
        // untouched, so regeneration adds no diff to unchanged cases.
        Trial::test("runner/update_keeps_matching_expected", || {
            let source = goldens_dir().join("stage0/promql_quantile_over_time");
            let dir = scratch_root().join("checks/keep_matching");
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir)?;
            for file in ["case.toml", "workload.json"] {
                std::fs::copy(source.join(file), dir.join(file))?;
            }
            run_case("runner/keep_matching", &dir, true)?;
            // The same document in other text: it matches, but a rewrite
            // would change the file.
            let compact = read_json(&dir.join("expected.json"))?.to_string();
            std::fs::write(dir.join("expected.json"), &compact)?;
            run_case("runner/keep_matching", &dir, true)?;
            if std::fs::read_to_string(dir.join("expected.json"))? == compact {
                Ok(())
            } else {
                Err("a matching expected.json was rewritten".into())
            }
        }),
        // A `runner/` directory of cases is refused, since its names would
        // collide with the runner's checks; `defaults/` holds no cases.
        Trial::test("runner/reserved_directories", || {
            let root = scratch_root().join("checks/reserved");
            let _ = std::fs::remove_dir_all(&root);
            for dir in ["defaults/x", "area/case"] {
                std::fs::create_dir_all(root.join(dir))?;
                std::fs::write(root.join(dir).join("case.toml"), "why = 'x'")?;
            }
            let names: Vec<String> = case_dirs(&root)?
                .into_iter()
                .map(|(name, _)| name)
                .collect();
            if names != ["area/case"] {
                return Err(format!("expected only area/case, found {names:?}").into());
            }
            std::fs::create_dir_all(root.join("runner/wrong_expected"))?;
            match case_dirs(&root) {
                Err(error) if error.contains("reserved") => Ok(()),
                other => Err(format!("a runner/ directory was accepted: {other:?}").into()),
            }
        }),
        // Two workload files with the same name, both without
        // `data_workload`, are planned as two distinct merged copies.
        Trial::test("runner/merged_workload_copies_do_not_collide", || {
            let dir = scratch_root().join("checks/same_name");
            let source = goldens_dir().join("stage0/promql_quantile_over_time/workload.json");
            for sub in ["a", "b"] {
                std::fs::create_dir_all(dir.join(sub))?;
                std::fs::copy(&source, dir.join(sub).join("workload.json"))?;
            }
            let case = parse_case("why = 'x'\nworkloads = ['a/workload.json', 'b/workload.json']")?;
            std::fs::create_dir_all(dir.join("scratch"))?;
            let command = planner_command(&case, &dir, &dir.join("scratch"))?;
            let workloads: Vec<&String> = command
                .windows(2)
                .filter(|pair| pair[0] == "--workload")
                .map(|pair| &pair[1])
                .collect();
            if workloads.len() == 2 && workloads[0] != workloads[1] {
                Ok(())
            } else {
                Err(format!("workload paths collide: {workloads:?}").into())
            }
        }),
        // The exit status must agree with the document: 1 exactly when a
        // stage records an error.
        Trial::test("runner/exit_status_agrees_with_document", || {
            let ok = serde_json::json!({"stage3_plan_selection": {"plan": {}}});
            let failed = serde_json::json!({"stage3_plan_selection": {"error": "infeasible"}});
            let agrees = |code, document| exit_status_agrees(code, document).is_ok();
            let all = agrees(Some(0), &ok)
                && agrees(Some(1), &failed)
                && !agrees(Some(1), &ok)
                && !agrees(Some(0), &failed)
                && !agrees(Some(2), &failed)
                && !agrees(None, &ok);
            all.then_some(())
                .ok_or_else(|| "exit status check failed".into())
        }),
        // A query listed as the root of two Stage 0 groups is an error, not
        // the first group found.
        Trial::test("runner/query_with_two_root_groups_is_an_error", || {
            let document = serde_json::json!({
                "workload": {"queries": [{"id": "q1"}]},
                "memo": {"steps": [{"step": "stage0", "groups": [
                    {"id": 0, "roots_of": [0]},
                    {"id": 1, "roots_of": [0]},
                ]}]},
            });
            match root_group(&document, "q1") {
                Err(error) if error.contains("2 stage0 groups") => Ok(()),
                other => Err(format!("expected an error, got {other:?}").into()),
            }
        }),
    ]
}
