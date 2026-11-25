//! An out-of-tree MetricsQL corpus: every query lowers or fails cleanly,
//! never panicking, and each file's lowered count stays at its floor. The
//! queries live outside this repository (e.g. a private corpus); the test
//! reads every `*.txt` in `$ASAP_EXTERNAL_CORPUS_DIR` and passes trivially
//! when it is unset, so CI still compiles it on every PR.
//!
//! Each file holds one query per line (`#` lines are comments) and a
//! `# floor: N` line: the minimum number of its queries that must lower.
//! Raise a floor to the current `lowered` count when coverage improves.
//!
//! `ASAP_EXTERNAL_CORPUS_DIR=<dir> cargo test -p asap-frontend-metricsql
//! --test external_corpus -- --nocapture`

use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use asap_frontend_metricsql::test_support::lower_metricsql_query;
use asap_frontend_metricsql::MetricsqlError;

const DIR_VAR: &str = "ASAP_EXTERNAL_CORPUS_DIR";

/// Every metric's sample interval, the lookback of a bare selector.
const INTERVAL: Duration = Duration::from_secs(15);

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
        match lower_metricsql_query(q, INTERVAL) {
            Ok(_) => t.lowered += 1,
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

/// Each `*.txt` in `dir` below its floor, as a message; prints every file's
/// tally and each failure cause with its count, most common first, to show
/// which unsupported features block the most queries.
fn check_dir(dir: &Path) -> Vec<String> {
    let mut files: Vec<_> = std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("{}: {e}", dir.display()))
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "txt"))
        .collect();
    files.sort();
    assert!(!files.is_empty(), "{} has no *.txt files", dir.display());

    let mut failures = Vec::new();
    let mut errors = HashMap::new();
    for path in files {
        let name = path.file_stem().unwrap().to_string_lossy();
        let corpus = std::fs::read_to_string(&path).unwrap();
        let min_lowered = floor(&corpus).unwrap_or_else(|| panic!("{name}: no `# floor: N` line"));
        let t = tally(&corpus, &mut errors);
        eprintln!("{name:24} {t:?}");
        if t.lowered < min_lowered {
            failures.push(format!("{name}: {t:?} (floor {min_lowered})"));
        }
    }
    let mut errors: Vec<_> = errors.into_iter().collect();
    errors.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    eprintln!("\nFailures by cause:");
    for (msg, n) in errors {
        eprintln!("{n:4}  {msg}");
    }
    failures
}

/// Lowering is total over every corpus file (a panic fails the test) and
/// each file lowers at least its floor.
#[test]
fn corpus_lowers() {
    let Some(dir) = std::env::var_os(DIR_VAR) else {
        eprintln!("{DIR_VAR} unset; skipping the external corpus");
        return;
    };
    let failures = check_dir(Path::new(&dir));
    assert!(failures.is_empty(), "{failures:#?}");
}

/// A file passes at its floor and fails above it; a parse failure counts
/// as unparseable, not lowered.
#[test]
fn floor_is_enforced_per_file() {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("external_corpus_sample");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let corpus = "rate(http_requests_total[5m])\nsum by (job) (up)\nsum((\n";
    std::fs::write(dir.join("at_floor.txt"), format!("# floor: 2\n{corpus}")).unwrap();
    std::fs::write(dir.join("above.txt"), format!("# floor: 3\n{corpus}")).unwrap();
    std::fs::write(dir.join("ignored.md"), "not a corpus").unwrap();

    let failures = check_dir(&dir);
    assert_eq!(failures.len(), 1, "{failures:#?}");
    assert!(failures[0].starts_with("above: "), "{failures:#?}");
    assert!(
        failures[0].contains("lowered: 2, unparseable: 1"),
        "{failures:#?}"
    );
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
