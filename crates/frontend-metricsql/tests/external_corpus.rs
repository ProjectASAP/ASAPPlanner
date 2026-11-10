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
