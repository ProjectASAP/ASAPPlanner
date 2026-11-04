//! Pass 2 builds on Pass 1, never the reverse (#1096): Pass 1's production
//! code does not import `crate::pass2`. Test modules may use Pass 2 to build
//! fixtures. Pass 2 → Pass 1 is still allowed (#1191).

use std::path::Path;

/// Pass 1's production source does not reference `pass2`.
#[test]
fn pass1_does_not_import_pass2() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = vec![src.join("pass1.rs")];
    for entry in std::fs::read_dir(src.join("pass1")).unwrap() {
        let path = entry.unwrap().path();
        // `*_tests.rs` files are `#[cfg(test)]` modules.
        if !path.to_string_lossy().ends_with("_tests.rs") {
            files.push(path);
        }
    }
    let mut offending = Vec::new();
    for file in files {
        let text = std::fs::read_to_string(&file).unwrap();
        // ponytail: a file's production code ends at its `#[cfg(test)] mod tests`;
        // parse with `syn` if inline `#[cfg(test)]` items ever need excluding.
        let production = text.split("#[cfg(test)]\nmod tests").next().unwrap();
        for (n, line) in production.lines().enumerate() {
            if line.contains("pass2") {
                offending.push(format!("{}:{}: {}", file.display(), n + 1, line.trim()));
            }
        }
    }
    assert!(
        offending.is_empty(),
        "Pass 1 imports Pass 2:\n{}",
        offending.join("\n")
    );
}
