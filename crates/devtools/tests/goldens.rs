//! The golden runner: one test per case directory under `tests/goldens/`
//! (design doc `testing-infrastructure`, D5–D8, D15, D25).
//!
//! A case directory holds a `case.toml` (spec in the design doc), its
//! workload files, optional `cost_inputs.json`, `deployment_capabilities.json`
//! and `tables/*.json`, and `expected.json`, the `asap_planner --format json`
//! output. The runner plans the case with the `asap_planner` binary and
//! compares its output with `expected.json`: numbers within a relative
//! tolerance of [`RELATIVE_TOLERANCE`], everything else exactly. A failure
//! prints a line diff and the command that reproduces it.
//!
//! `tests/goldens/defaults/` holds the inputs a case does not give: a case
//! file replaces the default file, and a workload without `data_workload`
//! gets `defaults/data_workload.json`.
//!
//! Run a subset by name: `cargo test --release -p asap-devtools --test
//! goldens stage0/`. Regenerate `expected.json` with `UPDATE_SNAPSHOTS=1`;
//! the case's assertions still run, so a wrong output is not written.

use std::path::{Path, PathBuf};
use std::process::Command;

use libtest_mimic::{Arguments, Failed, Trial};
use serde::Deserialize;
use serde_json::Value;

/// Numbers in the output match `expected.json` within this relative
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

/// Every case directory under `dir` (one holding a `case.toml`), with its
/// name relative to `tests/goldens/`, sorted.
fn case_dirs(dir: &Path, found: &mut Vec<(String, PathBuf)>) {
    if dir.join("case.toml").is_file() {
        let name = dir.strip_prefix(goldens_dir()).unwrap();
        found.push((name.to_string_lossy().replace('\\', "/"), dir.to_owned()));
        return;
    }
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.is_dir() && *path != defaults_dir())
        .collect();
    entries.sort();
    for entry in entries {
        case_dirs(&entry, found);
    }
}

fn main() {
    let arguments = Arguments::from_args();
    let update = std::env::var_os("UPDATE_SNAPSHOTS").is_some_and(|value| value == "1");
    let mut cases = Vec::new();
    case_dirs(&goldens_dir(), &mut cases);
    let mut trials: Vec<Trial> = cases
        .into_iter()
        .map(|(name, dir)| {
            Trial::test(name.clone(), move || {
                run_case(&name, &dir, update).map_err(Failed::from)
            })
        })
        .collect();
    trials.extend(runner_checks());
    libtest_mimic::run(&arguments, trials).exit();
}

/// Plans the case in `dir` and checks its assertions, then compares the
/// output with `expected.json`, or writes it there when `update`.
fn run_case(name: &str, dir: &Path, update: bool) -> Result<(), String> {
    let text = std::fs::read_to_string(dir.join("case.toml")).map_err(|e| e.to_string())?;
    let case = parse_case(&text).map_err(|e| format!("{name}/case.toml: {e}"))?;
    let scratch = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("goldens")
        .join(name);
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
        "output differs from expected.json (numbers within relative {RELATIVE_TOLERANCE}):\n{}\n{reproduce}",
        line_diff(&expected_text, &actual_text)
    ))
}

/// The `asap_planner` command line for `case`, with absolute paths: the
/// case's `args`, then its inputs, each from the case or the defaults.
fn planner_command(case: &Case, dir: &Path, scratch: &Path) -> Result<Vec<String>, String> {
    let mut command = vec![env!("CARGO_BIN_EXE_asap_planner").to_owned()];
    command.extend(case.args.iter().cloned());
    command.extend(["--format".into(), "json".into()]);
    for file in &case.workloads {
        command.push("--workload".into());
        command.push(workload_path(&dir.join(file), scratch)?);
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
/// `scratch` with `defaults/data_workload.json`. The copy outlives the
/// test, so the printed command reproduces the failure.
fn workload_path(path: &Path, scratch: &Path) -> Result<String, String> {
    let mut workload: Value = read_json(path)?;
    if !workload["data_workload"].is_null() {
        return Ok(path.display().to_string());
    }
    workload["data_workload"] = read_json(&defaults_dir().join("data_workload.json"))?;
    let copy = scratch.join(path.file_name().unwrap());
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
/// document records) still returns it; a run that wrote none fails.
fn run_planner(command: &[String], out: &Path) -> Result<Value, String> {
    let _ = std::fs::remove_file(out);
    let result = Command::new(&command[0])
        .args(&command[1..])
        .arg("--out")
        .arg(out)
        .output()
        .map_err(|e| format!("{}: {e}", command[0]))?;
    match std::fs::read_to_string(out) {
        Ok(text) => serde_json::from_str(&text).map_err(|e| format!("{}: {e}", out.display())),
        Err(_) => Err(format!(
            "asap_planner wrote no output ({}):\n{}",
            result.status,
            String::from_utf8_lossy(&result.stderr)
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
    stage0["groups"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|group| {
            group["roots_of"]
                .as_array()
                .is_some_and(|roots| roots.iter().any(|root| root.as_u64() == Some(index as u64)))
        })
        .and_then(|group| group["id"].as_u64())
        .ok_or_else(|| format!("no stage0 group is the root of {query}"))
}

/// Whether `actual` equals `expected`, numbers within [`RELATIVE_TOLERANCE`].
fn json_matches(expected: &Value, actual: &Value) -> bool {
    match (expected, actual) {
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
        // Numbers match within one unit in the 5th significant digit; other
        // values match exactly.
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
            let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("goldens-wrong-expected");
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
    ]
}
