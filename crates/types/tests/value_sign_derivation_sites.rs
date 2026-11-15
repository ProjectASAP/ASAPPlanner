//! Signs are derived in one place (sign test plan T4 b13): every
//! `ValueSign` a group's columns get is produced by the derivation in
//! `column_properties.rs`, once per memo group, or read from the
//! workload's declarations. Every consumer reads the stored property
//! (`column_signs`, `ValueSign::is_one_sign`,
//! `GroupProperties::state_column_sign`) and never recomputes it: a
//! second copy of a rule drifts from the first, as Stage 2's own lookup
//! of "the state column is the first non-plain field" could.
//!
//! The test scans the non-test Rust source of every crate under
//! `crates/*/src`: files under a `tests` or `test_support` directory,
//! files named `tests.rs` or `test_fixtures.rs`, and inline
//! `#[cfg(test)] mod … { … }` blocks are skipped.

use std::path::{Path, PathBuf};

/// The derivation's file, relative to the workspace root.
const DERIVATION: &str = "crates/types/src/stages/logical/group/column_properties.rs";

/// Functions outside [`DERIVATION`] allowed to return a `ValueSign`: each
/// reads a stored sign, none derives one. `(file, function name)`.
const SIGN_READERS: &[(&str, &str)] = &[(
    "crates/types/src/stages/logical/group/mod.rs",
    "state_column_sign",
)];

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("the workspace root")
}

/// Every `.rs` file below `directory`, but test files and directories.
fn non_test_rust_files(directory: &Path, files: &mut Vec<PathBuf>) {
    let mut entries: Vec<_> = std::fs::read_dir(directory)
        .unwrap_or_else(|error| panic!("{}: {error}", directory.display()))
        .map(|entry| entry.unwrap().path())
        .collect();
    entries.sort();
    for path in entries {
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        if path.is_dir() {
            if name != "tests" && name != "test_support" {
                non_test_rust_files(&path, files);
            }
        } else if name.ends_with(".rs") && name != "tests.rs" && name != "test_fixtures.rs" {
            files.push(path);
        }
    }
}

/// The text from `open`, the index of a `{`, through its matching `}`.
fn braced_block_end(text: &str, open: usize) -> usize {
    let mut depth = 0usize;
    for (offset, byte) in text.as_bytes()[open..].iter().enumerate() {
        match byte {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return open + offset + 1;
                }
            }
            _ => {}
        }
    }
    text.len()
}

/// `text` without its inline `#[cfg(test)] mod name { … }` blocks: a
/// `#[cfg(test)] mod tests;` declaration (its file is skipped by name)
/// stays.
fn without_inline_test_modules(text: &str) -> String {
    let mut kept = String::new();
    let mut rest = text;
    while let Some(attribute) = rest.find("#[cfg(test)]") {
        let after = &rest[attribute + "#[cfg(test)]".len()..];
        let trimmed = after.trim_start();
        let inline_module = trimmed.starts_with("mod ")
            && trimmed
                .find(['{', ';'])
                .is_some_and(|end| trimmed.as_bytes()[end] == b'{');
        if !inline_module {
            kept.push_str(&rest[..attribute + "#[cfg(test)]".len()]);
            rest = after;
            continue;
        }
        kept.push_str(&rest[..attribute]);
        let module_start = rest.len() - trimmed.len();
        let open = module_start + trimmed.find('{').unwrap();
        rest = &rest[braced_block_end(rest, open)..];
    }
    kept.push_str(rest);
    kept
}

/// The non-test source of each crate file, keyed by its path relative to
/// the workspace root (`crates/<crate>/src/…`).
fn non_test_sources() -> Vec<(String, String)> {
    let root = workspace_root();
    let mut files = Vec::new();
    let mut crates: Vec<_> = std::fs::read_dir(root.join("crates"))
        .unwrap()
        .map(|entry| entry.unwrap().path().join("src"))
        .filter(|src| src.is_dir())
        .collect();
    crates.sort();
    for src in crates {
        non_test_rust_files(&src, &mut files);
    }
    files
        .into_iter()
        .map(|path| {
            let text = std::fs::read_to_string(&path).unwrap();
            let relative = path
                .strip_prefix(&root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            (relative, without_inline_test_modules(&text))
        })
        .collect()
}

/// The names of `source`'s functions whose return type mentions
/// `ValueSign` (`fn agree(a: ValueSign, b: ValueSign) -> ValueSign`,
/// `-> Option<ValueSign>`, `-> Vec<ValueSign>`).
fn functions_returning_a_sign(source: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut rest = source;
    while let Some(start) = rest.find("fn ") {
        let preceded_by_identifier = start > 0 && {
            let before = rest.as_bytes()[start - 1];
            before.is_ascii_alphanumeric() || before == b'_'
        };
        let signature_and_rest = &rest[start + 3..];
        rest = signature_and_rest;
        if preceded_by_identifier {
            continue;
        }
        let end = signature_and_rest
            .find(['{', ';'])
            .unwrap_or(signature_and_rest.len());
        let signature = &signature_and_rest[..end];
        let name: String = signature
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect();
        if signature
            .split_once("->")
            .is_some_and(|(_, returns)| returns.contains("ValueSign"))
        {
            found.push(name);
        }
    }
    found
}

/// The body of `fn name` in `source`, or `None` when it has none.
fn function_body<'a>(source: &'a str, name: &str) -> Option<&'a str> {
    let start = source.find(&format!("fn {name}("))?;
    let open = start + source[start..].find('{')?;
    Some(&source[open..braced_block_end(source, open)])
}

/// No function outside the derivation produces a `ValueSign` but the
/// listed readers of a stored one; the error contract's
/// `values_of_one_sign` (if it is kept) asks `is_one_sign`, not its own
/// `NonNegative | NonPositive` match; Stage 2's Count-Min check and the
/// DDSketch relative gate read `state_column_sign`, and Stage 2 indexes
/// no `column_signs` itself. Every violation is listed, then the test
/// fails once with all of them. Today Stage 2 looks the state column up
/// inline and `values_of_one_sign` matches the two signs itself.
#[test]
fn b13_no_sign_is_derived_outside_the_derivation() {
    let sources = non_test_sources();
    assert!(
        sources.iter().any(|(path, _)| path == DERIVATION),
        "the derivation file is scanned"
    );
    assert!(
        sources
            .iter()
            .any(|(path, source)| path == DERIVATION
                && !functions_returning_a_sign(source).is_empty()),
        "the scanner finds the derivation's own sign functions"
    );
    let mut violations = Vec::new();
    for (path, source) in &sources {
        if path == DERIVATION {
            continue;
        }
        for name in functions_returning_a_sign(source) {
            if !SIGN_READERS.contains(&(path.as_str(), name.as_str())) {
                violations.push(format!("{path}: fn {name} returns a ValueSign"));
            }
        }
    }
    let crate_source = |prefix: &str| -> String {
        sources
            .iter()
            .filter(|(path, _)| path.starts_with(prefix))
            .map(|(_, source)| source.as_str())
            .collect()
    };
    let stage2 = crate_source("crates/stage2-physical-optimization/src/");
    if !stage2.contains("state_column_sign(") {
        violations.push("Stage 2 does not call state_column_sign".into());
    }
    if stage2.contains("column_signs") {
        violations.push("Stage 2 reads column_signs itself".into());
    }
    // provisional answer Q7: the DDSketch relative gate is at the logical
    // edge, in the accuracy crate, and reads state_column_sign.
    if !crate_source("crates/accuracy/src/").contains("state_column_sign(") {
        violations
            .push("the accuracy crate's DDSketch gate does not call state_column_sign".into());
    }
    let error_contract = crate_source("crates/accuracy/src/error_contract/");
    if let Some(body) = function_body(&error_contract, "values_of_one_sign") {
        if !body.contains("is_one_sign") {
            violations.push("values_of_one_sign does not call is_one_sign".into());
        }
    }
    assert!(violations.is_empty(), "{violations:#?}");
}

/// The scanner's own helpers on hand-written source, so a parsing slip
/// does not pass the scan above vacuously.
#[test]
fn the_derivation_site_scanner_reads_signatures_and_skips_test_modules() {
    let source = "fn agree(a: ValueSign) -> ValueSign { a }\n\
                  pub fn state_column_sign(&self) -> Option<ValueSign> { None }\n\
                  fn is_one_sign(self) -> bool { true }\n\
                  fn reads(sign: ValueSign) {}\n\
                  #[cfg(test)]\nmod tests { fn inside() -> ValueSign { todo!() } }\n\
                  #[cfg(test)]\nmod external;\n";
    let stripped = without_inline_test_modules(source);
    assert!(!stripped.contains("inside"));
    assert!(stripped.contains("mod external;"));
    assert_eq!(
        functions_returning_a_sign(&stripped),
        vec!["agree".to_string(), "state_column_sign".to_string()]
    );
    assert_eq!(
        function_body(
            "fn values_of_one_sign(x: u8) -> bool { x.is_one_sign() }",
            "values_of_one_sign"
        ),
        Some("{ x.is_one_sign() }")
    );
}
