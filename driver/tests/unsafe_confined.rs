//! NFR-MAINT-05: the product's sources (`core/src`, `driver/src`) allow
//! `unsafe_code` in one place only, the `sys` module's declaration in
//! `driver/src/lib.rs`. The workspace lint denies it everywhere else, and
//! this test makes sure no second `allow` (or `expect`) of it appears.
//! Every `.rs` file under those trees is scanned, unit tests included.
//! The driver's test sources (`driver/tests`) use the `unsafe` keyword
//! in two places only: the `pre_exec` module of `support/child.rs` and
//! the clock shim `support/clock_syscall.rs`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "test helpers outside #[test] functions; clippy.toml allows these in tests"
)]

use std::fs;
use std::path::{Path, PathBuf};

/// Every `.rs` file under `dir`, in a stable order.
fn sources(dir: &Path, out: &mut Vec<PathBuf>) {
    let mut entries: Vec<PathBuf> = fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            sources(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
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
                found.push(text[..from + at].matches('\n').count() + 1);
            }
            from = start;
        }
    }
    found.sort_unstable();
    found
}

/// The line numbers (from 1) of every `unsafe` keyword outside `//`
/// comments.
fn unsafe_keywords(text: &str) -> Vec<usize> {
    let keyword = concat!("un", "safe");
    text.lines()
        .enumerate()
        .filter(|(_, line)| {
            let code = line.split("//").next().unwrap_or_default();
            code.split(|c: char| !(c.is_alphanumeric() || c == '_'))
                .any(|word| word == keyword)
        })
        .map(|(index, _)| index + 1)
        .collect()
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

#[test]
fn test_code_uses_unsafe_only_in_its_two_places() {
    let tests = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
    let mut files = Vec::new();
    sources(&tests, &mut files);
    let child = tests.join("support/child.rs");
    let shim = tests.join("support/clock_syscall.rs");
    let child_text = fs::read_to_string(&child).unwrap();
    let lines: Vec<&str> = child_text.lines().collect();
    let start = lines
        .iter()
        .position(|line| *line == "mod pre_exec {")
        .expect("the pre_exec module in support/child.rs");
    let end = start
        + lines[start..]
            .iter()
            .position(|line| *line == "}")
            .expect("the end of the pre_exec module");
    let pre_exec = start + 1..=end + 1;
    let (mut in_pre_exec, mut in_shim, mut stray) = (0, 0, Vec::new());
    for file in &files {
        for line in unsafe_keywords(&fs::read_to_string(file).unwrap()) {
            if *file == shim {
                in_shim += 1;
            } else if *file == child && pre_exec.contains(&line) {
                in_pre_exec += 1;
            } else {
                let relative = file.strip_prefix(&tests).unwrap().display().to_string();
                stray.push(format!("{relative}:{line}"));
            }
        }
    }
    assert!(in_pre_exec > 0 && in_shim > 0, "{in_pre_exec} {in_shim}");
    assert!(stray.is_empty(), "stray: {stray:?}");
}

#[test]
fn the_keyword_is_found_outside_comments_only() {
    let keyword = concat!("un", "safe");
    let text = format!("// {keyword}\nlet a = {keyword} {{ f() }};\nfn {keyword}_x() {{}}\n");
    assert_eq!(unsafe_keywords(&text), [2]);
}
