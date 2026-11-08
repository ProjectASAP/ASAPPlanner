//! An out-of-tree MetricsQL corpus: every query lowers and binds without
//! panicking. The queries live outside this repository (e.g. a private
//! corpus); the test reads every `*.txt` in `$ASAP_EXTERNAL_CORPUS_DIR` and
//! passes trivially when it is unset, so CI still compiles it on every PR.
//!
//! Each file holds one query per line (`#` lines are comments) and a
//! `# floor: N` line: the minimum number of its queries that must lower.
//! Raise a floor to the current `lowered` count when coverage improves.
//!
//! `ASAP_EXTERNAL_CORPUS_DIR=<dir> cargo test -p asap-frontend-metricsql
//! --test metricsql external_corpus:: -- --nocapture`

use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use asap_frontend_metricsql::{lower_metricsql_query, MetricsqlError};
use asap_types::ir::QueryRoot;

use crate::support::logical_plan;

const DIR_VAR: &str = "ASAP_EXTERNAL_CORPUS_DIR";

fn queries(corpus: &str) -> impl Iterator<Item = &str> {
    corpus
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
}

/// The `# floor: N` line's `N`.
fn floor(corpus: &str) -> Option<usize> {
    corpus
        .lines()
        .find_map(|l| l.trim().strip_prefix("# floor:"))
        .and_then(|n| n.trim().parse().ok())
}

#[derive(Default, Debug)]
struct Tally {
    lowered: usize,
    unparseable: usize,
    rejected: usize,
    bind_errored: usize,
}

/// `msg` with sanitized identifiers replaced by `_` and digit runs by `N`, so
/// the same failure in different queries groups under one message.
fn normalize(msg: &str) -> String {
    let mut out = String::new();
    let mut word = String::new();
    for c in msg.chars().chain([' ']) {
        if c.is_alphanumeric() || c == '_' {
            word.push(c);
            continue;
        }
        if word.starts_with("sanitized_") {
            out.push('_');
        } else if !word.is_empty() && word.chars().all(|c| c.is_ascii_digit()) {
            out.push('N');
        } else {
            out.push_str(&word);
        }
        word.clear();
        out.push(c);
    }
    out.pop();
    out
}

/// Tally `corpus`, counting each failure's normalized message in `errors`.
fn tally(corpus: &str, errors: &mut HashMap<String, usize>) -> Tally {
    let mut t = Tally::default();
    for q in queries(corpus) {
        match lower_metricsql_query(q, Duration::from_secs(15)) {
            Ok(QueryRoot::Operator(dag)) => {
                t.lowered += 1;
                if logical_plan(&dag).is_err() {
                    t.bind_errored += 1;
                }
            }
            Ok(QueryRoot::Scalar(_)) => t.lowered += 1,
            Err(e) => {
                if matches!(e, MetricsqlError::Parse(_)) {
                    t.unparseable += 1;
                } else {
                    t.rejected += 1;
                }
                *errors.entry(normalize(&e.to_string())).or_default() += 1;
            }
        }
    }
    t
}

/// Lowering and binding are total over every corpus file: a panic fails the
/// test, no lowerable query fails to bind, and coverage stays at its floor.
/// Also prints each unparseable/rejected cause with its count, most common
/// first, to show which unsupported features block the most queries.
#[test]
fn corpus_lowers_and_binds() {
    let Some(dir) = std::env::var_os(DIR_VAR) else {
        eprintln!("{DIR_VAR} unset; skipping the external corpus");
        return;
    };
    let mut files: Vec<_> = std::fs::read_dir(Path::new(&dir))
        .unwrap_or_else(|e| panic!("{DIR_VAR}: {e}"))
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "txt"))
        .collect();
    files.sort();
    assert!(!files.is_empty(), "{DIR_VAR} has no *.txt files");

    let mut failures = Vec::new();
    let mut errors = HashMap::new();
    for path in files {
        let name = path.file_stem().unwrap().to_string_lossy();
        let corpus = std::fs::read_to_string(&path).unwrap();
        let min_lowered = floor(&corpus).unwrap_or_else(|| panic!("{name}: no `# floor: N` line"));
        let t = tally(&corpus, &mut errors);
        eprintln!("{name:24} {t:?}");
        if t.bind_errored > 0 || t.lowered < min_lowered {
            failures.push(format!("{name}: {t:?} (floor {min_lowered})"));
        }
    }
    let mut errors: Vec<_> = errors.into_iter().collect();
    errors.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    eprintln!("\nFailures by cause:");
    for (msg, n) in errors {
        eprintln!("{n:4}  {msg}");
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

#[test]
fn floor_line() {
    assert_eq!(floor("# floor: 25\nrate(x[5m])"), Some(25));
    assert_eq!(floor("rate(x[5m])"), None);
    assert_eq!(
        queries("# floor: 3\n\n up \n# c").collect::<Vec<_>>(),
        ["up"]
    );
    assert_eq!(normalize("sanitized_foo at 123"), "_ at N");
}
