//! Completion oracle for the ClimateTuning tearout: "gone" is measured, not
//! asserted. Climate — the module, the type, the tuning knobs, the design
//! rationale — must leave zero trace anywhere in source, tests, or docs. A
//! stray `pub use`, a forgotten doc paragraph, a leftover `retune_climate`
//! call: any of those is exactly the kind of partial removal that made this
//! thing sticky across previous attempts, and this test exists to make that
//! structurally impossible to miss.
//!
//! Scans `src/`, `tests/` (excluding this file), `docs/`, and `README.md`
//! for the case-insensitive substring "climate". Fails if it finds any.

use std::fs;
use std::path::{Path, PathBuf};

fn scan_dir(dir: &Path, self_path: &Path, hits: &mut Vec<String>) {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return, // directory doesn't exist -- nothing to scan
    };
    for entry in entries {
        let path = entry.expect("readable dir entry").path();
        if path.is_dir() {
            if path.file_name().is_some_and(|n| n == "target") {
                continue;
            }
            scan_dir(&path, self_path, hits);
            continue;
        }
        let is_relevant = matches!(path.extension().and_then(|e| e.to_str()), Some("rs") | Some("md"));
        if !is_relevant || path == self_path {
            continue;
        }
        scan_file(&path, hits);
    }
}

fn scan_file(path: &Path, hits: &mut Vec<String>) {
    let text = fs::read_to_string(path).unwrap_or_default();
    for (i, line) in text.lines().enumerate() {
        if line.to_lowercase().contains("climate") {
            hits.push(format!("{}:{}: {}", path.display(), i + 1, line.trim()));
        }
    }
}

#[test]
fn climate_leaves_no_trace_anywhere_in_the_tree() {
    let self_path = PathBuf::from(file!());
    let mut hits = Vec::new();

    for dir in ["src", "tests", "docs"] {
        scan_dir(Path::new(dir), &self_path, &mut hits);
    }
    scan_file(Path::new("README.md"), &mut hits);

    assert!(
        hits.is_empty(),
        "climate was supposed to be torn out entirely -- still found in {} place(s):\n{}",
        hits.len(),
        hits.join("\n")
    );
}
