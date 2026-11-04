//! Stage 1's passes are independent modules (#1096, #1191): neither pass's
//! production code imports the other. Test modules may use the other pass to
//! build fixtures. What both passes need lives in `crate::ir_construction`.

use std::path::{Path, PathBuf};

/// Production lines under `src/<pass>.rs` and `src/<pass>/` that name a path
/// through `needle`'s module.
fn production_lines_mentioning(pass: &str, needle: &str) -> Vec<String> {
    fn walk(dir: &Path, files: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(&path, files);
            // `*_tests.rs` files are `#[cfg(test)]` modules.
            } else if !path.to_string_lossy().ends_with("_tests.rs") {
                files.push(path);
            }
        }
    }
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = vec![src.join(format!("{pass}.rs"))];
    walk(&src.join(pass), &mut files);
    let mut offending = Vec::new();
    for file in files {
        let text = std::fs::read_to_string(&file).unwrap();
        // ponytail: a file's production code ends at its `#[cfg(test)] mod tests`;
        // parse with `syn` if inline `#[cfg(test)]` items ever need excluding.
        let production = text.split("#[cfg(test)]\nmod tests").next().unwrap();
        for (n, line) in production.lines().enumerate() {
            if line.contains(needle) {
                offending.push(format!("{}:{}: {}", file.display(), n + 1, line.trim()));
            }
        }
    }
    offending
}

/// Pass 1's production source does not reference `pass2`.
#[test]
fn pass1_does_not_import_pass2() {
    let offending = production_lines_mentioning("pass1", "pass2::");
    assert!(
        offending.is_empty(),
        "Pass 1 imports Pass 2:\n{}",
        offending.join("\n")
    );
}

/// Pass 2's production source does not reference `pass1`.
#[test]
fn pass2_does_not_import_pass1() {
    let offending = production_lines_mentioning("pass2", "pass1::");
    assert!(
        offending.is_empty(),
        "Pass 2 imports Pass 1:\n{}",
        offending.join("\n")
    );
}
