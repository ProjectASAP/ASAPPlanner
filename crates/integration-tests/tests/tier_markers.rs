//! The tier markers (`docs/develop_docs/testing.md`, "Tiers"): a test of the
//! full or nightly tier carries both an ignore reason starting with its
//! tier and a module of the same name, so `cargo test -- --ignored
//! <tier>_tier` selects it; this keeps the two from drifting apart.

use std::path::{Path, PathBuf};

/// Every `.rs` file under `directory`, skipping build output.
fn rust_files(directory: &Path, files: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(directory).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            if path.file_name().is_some_and(|name| name != "target") {
                rust_files(&path, files);
            }
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            files.push(path);
        }
    }
}

/// The leading whitespace of `line`.
fn indent(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

/// The module enclosing line `at` of `lines`: the nearest `mod <name> {`
/// above it indented less (rustfmt indents each module's body one level),
/// or `None` at the file's top level.
fn enclosing_module<'a>(lines: &[&'a str], at: usize) -> Option<&'a str> {
    let depth = indent(lines[at]);
    lines[..at].iter().rev().find_map(|line| {
        let declaration = line.trim_start();
        let declaration = declaration.strip_prefix("pub ").unwrap_or(declaration);
        let name = declaration.strip_prefix("mod ")?.strip_suffix(" {")?;
        (indent(line) < depth).then_some(name)
    })
}

/// Every ignore reason that starts `full tier:` or `nightly tier:` in the
/// workspace's crates sits in a module named `full_tier` or
/// `nightly_tier`.
#[test]
fn every_tier_ignore_sits_in_its_tier_module() {
    let crates = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
    let mut files = Vec::new();
    rust_files(&crates, &mut files);
    let mut misplaced = Vec::new();
    for file in files {
        let text = std::fs::read_to_string(&file).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        for (at, line) in lines.iter().enumerate() {
            for tier in ["full", "nightly"] {
                if !line.contains(&format!("#[ignore = \"{tier} tier:")) {
                    continue;
                }
                let module = enclosing_module(&lines, at);
                if module != Some(&format!("{tier}_tier")) {
                    misplaced.push(format!(
                        "{}:{}: a {tier} tier ignore in module {module:?}",
                        file.display(),
                        at + 1
                    ));
                }
            }
        }
    }
    assert!(misplaced.is_empty(), "{misplaced:#?}");
}

/// The enclosing module is the nearest less-indented `mod … {` above.
#[test]
fn the_enclosing_module_is_read_from_indentation() {
    let lines = [
        "mod outer {",
        "    mod closed {",
        "    }",
        "    #[test]",
        "}",
        "#[test]",
    ];
    assert_eq!(enclosing_module(&lines, 3), Some("outer"));
    assert_eq!(enclosing_module(&lines, 5), None);
}
