//! NFR-MAINT-05: the product's sources (`core/src`, `driver/src`) allow
//! `unsafe_code` in one place only, the `sys` module's declaration in
//! `driver/src/lib.rs`. The workspace lint denies it everywhere else, and
//! this test makes sure no second `allow` (or `expect`) of it appears.
//! Test code is excluded: the `tests.rs` files of the product sources
//! (the test crate's own `pre_exec` module is outside these trees).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "test helpers outside #[test] functions; clippy.toml allows these in tests"
)]

use std::fs;
use std::path::{Path, PathBuf};

/// Every `.rs` file under `dir`, test files excluded, in a stable order.
fn sources(dir: &Path, out: &mut Vec<PathBuf>) {
    let mut entries: Vec<PathBuf> = fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            sources(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs")
            && path.file_name().is_some_and(|n| n != "tests.rs")
        {
            out.push(path);
        }
    }
}

/// The line numbers (from 1) of every `allow(...)` or `expect(...)` whose
/// arguments name `unsafe_code`, an attribute spanning lines included
/// (reported at its first line).
fn unsafe_allows(text: &str) -> Vec<usize> {
    let mut found = Vec::new();
    for opener in ["allow(", "expect("] {
        let mut from = 0;
        while let Some(at) = text[from..].find(opener) {
            let start = from + at + opener.len();
            let mut depth = 1;
            let mut end = start;
            for (i, c) in text[start..].char_indices() {
                match c {
                    '(' => depth += 1,
                    ')' => depth -= 1,
                    _ => {}
                }
                if depth == 0 {
                    end = start + i;
                    break;
                }
            }
            let names_it = text[start..end]
                .split(|c: char| !(c.is_alphanumeric() || c == '_'))
                .any(|word| word == "unsafe_code");
            if names_it {
                found.push(text[..from + at].lines().count().max(1));
            }
            from = start;
        }
    }
    found.sort_unstable();
    found
}

#[test]
fn unsafe_code_is_allowed_only_for_sys() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let mut files = Vec::new();
    sources(&root.join("core/src"), &mut files);
    sources(&root.join("driver/src"), &mut files);
    assert!(
        files.len() > 20,
        "the source trees were not found: {files:?}"
    );
    let mut hits = Vec::new();
    for file in &files {
        let text = fs::read_to_string(file).unwrap();
        for line in unsafe_allows(&text) {
            let relative = file.strip_prefix(&root).unwrap().display().to_string();
            hits.push(format!("{relative}:{line}"));
        }
    }
    let lib = fs::read_to_string(root.join("driver/src/lib.rs")).unwrap();
    let lines: Vec<&str> = lib.lines().collect();
    let sys = lines
        .iter()
        .position(|line| *line == "pub mod sys;")
        .expect("the sys module in driver/src/lib.rs");
    assert_eq!(
        lines.get(sys.wrapping_sub(1)),
        Some(&"#[allow(unsafe_code)]"),
        "the allow is on the sys module"
    );
    assert_eq!(hits, [format!("driver/src/lib.rs:{sys}")]);
}

#[test]
fn an_allow_is_found_on_one_line_or_across_lines() {
    assert_eq!(unsafe_allows("#[allow(unsafe_code)]\nmod a;"), [1]);
    assert_eq!(
        unsafe_allows("fn f() {}\n#![expect(\n    clippy::x,\n    unsafe_code,\n)]\n"),
        [2]
    );
    assert_eq!(unsafe_allows("#[allow(clippy::unsafe_code_x)]"), [0; 0]);
    assert_eq!(unsafe_allows("#![deny(unsafe_code)]"), [0; 0]);
}
