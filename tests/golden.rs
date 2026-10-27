//! Golden files: real and deliberately awkward pyproject.toml inputs, edited
//! against a fixed version grid so the bytes do not depend on PyPI.
//!
//! The grid tops out at `99.0.0` (see `fixed_test_grid`). Versions in
//! `tests/fixtures/expected/` are that grid, not current PyPI releases.

use std::fs;
use std::path::Path;
use uv_bump::{bump_str, is_single_specifier, preserves_unrelated_bytes, GridIndex, Options};

fn cases() -> Vec<(&'static str, &'static str)> {
    vec![
        (
            "tests/fixtures/crafted/odd.toml",
            "tests/fixtures/expected/odd.toml",
        ),
        (
            "tests/fixtures/real/httpx.toml",
            "tests/fixtures/expected/httpx.toml",
        ),
        (
            "tests/fixtures/real/flask.toml",
            "tests/fixtures/expected/flask.toml",
        ),
        (
            "tests/fixtures/real/fastapi.toml",
            "tests/fixtures/expected/fastapi.toml",
        ),
        (
            "tests/fixtures/real/pydantic.toml",
            "tests/fixtures/expected/pydantic.toml",
        ),
        (
            "tests/fixtures/real/ruff.toml",
            "tests/fixtures/expected/ruff.toml",
        ),
        (
            "tests/fixtures/real/rich.toml",
            "tests/fixtures/expected/rich.toml",
        ),
    ]
}

#[test]
fn golden_files_match_and_only_specifiers_change() {
    let update = std::env::var("UV_BUMP_UPDATE_GOLDEN").ok().as_deref() == Some("1");
    for (input_path, expected_path) in cases() {
        let input = fs::read_to_string(input_path).unwrap_or_else(|err| {
            panic!("read {input_path}: {err}");
        });
        let outcome = bump_str(&input, &GridIndex, &Options::default()).unwrap_or_else(|err| {
            panic!("bump {input_path}: {err}");
        });
        assert!(
            preserves_unrelated_bytes(&input, &outcome.updated, &outcome.edits),
            "{input_path} changed bytes outside specifier edits"
        );
        for edit in &outcome.edits {
            assert!(
                is_single_specifier(&edit.original),
                "{input_path} edit original {:?} is not one specifier",
                edit.original
            );
            assert!(
                is_single_specifier(&edit.replacement),
                "{input_path} edit replacement {:?} is not one specifier",
                edit.replacement
            );
        }
        let unparseable: Vec<_> = outcome
            .notes
            .iter()
            .filter(|note| note.reason == "unparseable")
            .map(|note| format!("{}: {}", note.group, note.requirement))
            .collect();
        assert!(
            unparseable.is_empty(),
            "{input_path} has unparseable requirements: {unparseable:?}"
        );
        if update {
            if let Some(parent) = Path::new(expected_path).parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(expected_path, &outcome.updated).unwrap();
            continue;
        }
        let expected = fs::read_to_string(expected_path).unwrap_or_else(|err| {
            panic!("read {expected_path}: {err} (set UV_BUMP_UPDATE_GOLDEN=1 to create it)");
        });
        assert_eq!(
            outcome.updated, expected,
            "{input_path} does not match {expected_path}"
        );
    }
}
