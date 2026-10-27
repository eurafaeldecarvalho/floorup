//! Bump dependency bounds in a `pyproject.toml` without reformatting it.
//!
//! The CLI in `main.rs` looks versions up on PyPI. Tests and other callers
//! pass a [`VersionSource`] so the edit itself can be checked without a network.

#![forbid(unsafe_code)]

mod diff;
mod edit;
mod parse;
mod policy;
mod pypi;

pub use diff::unified;
pub use edit::{preserves_unrelated_bytes, FileEdit};
pub use parse::{is_single_specifier, normalize_name};
pub use pypi::{
    default_cache_dir, json_endpoint, parse_pypi_json, redact, LookupError, MapIndex, PypiConfig,
    PypiIndex, Release, VersionSource,
};

use edit::locate;
use parse::{parse_requirement, Op, Requirement};
use policy::{decide, Decision, Lookup, Policy};
use serde::Serialize;
use std::collections::BTreeSet;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Message(String),
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Options {
    pub allow_pre: bool,
    pub bump_pins: bool,
    pub expand_compatible: bool,
    pub include_packages: BTreeSet<String>,
    pub exclude_packages: BTreeSet<String>,
    pub include_groups: BTreeSet<String>,
    pub exclude_groups: BTreeSet<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Change {
    pub name: String,
    pub group: String,
    pub before: String,
    pub after: String,
    pub line: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Note {
    pub name: String,
    pub group: String,
    pub requirement: String,
    pub reason: String,
    pub detail: String,
    pub line: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub updated: String,
    pub changes: Vec<Change>,
    pub notes: Vec<Note>,
    pub already_current: usize,
    pub unbounded: usize,
    pub edits: Vec<FileEdit>,
}

/// Edit `text` using versions from `index`.
///
/// On success the returned document is the original text with specifier slices
/// replaced. Nothing is written to disk.
pub fn bump_str(text: &str, index: &dyn VersionSource, opts: &Options) -> Result<Outcome, Error> {
    let located = locate(text)?;
    let policy = Policy {
        allow_pre: opts.allow_pre,
        bump_pins: opts.bump_pins,
        expand_compatible: opts.expand_compatible,
    };

    struct Planned {
        filter_id: String,
        group_label: String,
        decoded: String,
        value_span: std::ops::Range<usize>,
        line: usize,
        requirement: Requirement,
    }

    let mut planned = Vec::new();
    let mut notes = Vec::new();

    for item in located {
        if !group_selected(&item.filter_id, opts) {
            continue;
        }
        let line = line_number(text, item.value_span.start);
        let requirement = match parse_requirement(&item.decoded) {
            Ok(requirement) => requirement,
            Err(err) => {
                let name = guess_name(&item.decoded);
                if package_selected(&name, opts) {
                    notes.push(Note {
                        name,
                        group: item.group_label.clone(),
                        requirement: item.decoded.trim().to_string(),
                        reason: policy::Reason::Unparseable.as_str().to_string(),
                        detail: err.message,
                        line,
                    });
                }
                continue;
            }
        };
        if !package_selected(&requirement.name, opts) {
            continue;
        }
        planned.push(Planned {
            filter_id: item.filter_id,
            group_label: item.group_label,
            decoded: item.decoded,
            value_span: item.value_span,
            line,
            requirement,
        });
    }

    let mut names = Vec::new();
    for item in &planned {
        if needs_lookup(&item.requirement, opts) {
            names.push(normalize_name(&item.requirement.name));
        }
    }
    names.sort();
    names.dedup();
    index.prefetch(&names)?;

    let mut edits = Vec::new();
    let mut changes = Vec::new();
    let mut already_current = 0usize;
    let mut unbounded = 0usize;

    for item in &planned {
        let lookup = if needs_lookup(&item.requirement, opts) {
            Some(
                match index.releases(&normalize_name(&item.requirement.name)) {
                    Ok(releases) => Lookup::Found(releases),
                    Err(LookupError::NotFound) => Lookup::NotFound,
                    Err(LookupError::Failed(message)) => Lookup::Failed(message),
                },
            )
        } else {
            None
        };
        // Requirements that do not need a lookup still go through `decide`,
        // which classifies pins, bare names, and direct references. Those
        // paths return before reading `lookup`.
        let decision = if lookup.is_none() {
            decide_without_lookup(&item.requirement, policy)
        } else {
            decide(&item.requirement, lookup, policy)
        };

        match decision {
            Decision::Bump(decoded_edits) => {
                let located = edit::Located {
                    filter_id: item.filter_id.clone(),
                    group_label: item.group_label.clone(),
                    decoded: item.decoded.clone(),
                    value_span: item.value_span.clone(),
                };
                let mut file_edits = Vec::new();
                let mut failed = false;
                for decoded in &decoded_edits {
                    match edit::map_edit(text, &located, decoded) {
                        Some(file_edit) => file_edits.push(file_edit),
                        None => {
                            failed = true;
                            break;
                        }
                    }
                }
                if failed {
                    notes.push(make_note(
                        &item.requirement.name,
                        &item.group_label,
                        &item.decoded,
                        item.line,
                        policy::Reason::CouldNotEdit,
                        "could not map the specifier back onto the original TOML string",
                    ));
                    continue;
                }
                let after = apply_decoded(&item.decoded, &decoded_edits);
                changes.push(Change {
                    name: item.requirement.name.clone(),
                    group: item.group_label.clone(),
                    before: item.decoded.trim().to_string(),
                    after: after.trim().to_string(),
                    line: item.line,
                });
                edits.extend(file_edits);
            }
            Decision::Current => already_current += 1,
            Decision::Unbounded => unbounded += 1,
            Decision::Note(reason, detail) => notes.push(make_note(
                &item.requirement.name,
                &item.group_label,
                &item.decoded,
                item.line,
                reason,
                &detail,
            )),
        }
    }

    let updated = edit::apply_edits(text, &edits)?;
    Ok(Outcome {
        updated,
        changes,
        notes,
        already_current,
        unbounded,
        edits,
    })
}

fn decide_without_lookup(req: &Requirement, policy: Policy) -> Decision {
    // `decide` only reads the release list after it has classified skips.
    // Passing an empty found-list would turn a pin into "no eligible release",
    // so pins and bare names are classified here.
    if req.is_url {
        return Decision::Note(
            policy::Reason::DirectReference,
            "direct URL, path, or VCS requirement".to_string(),
        );
    }
    if req.specifiers.iter().any(|spec| spec.op == Op::Arbitrary) {
        return Decision::Note(
            policy::Reason::ArbitraryEquality,
            "=== is an arbitrary string match and is not rewritten".to_string(),
        );
    }
    let pinned = req.specifiers.iter().any(|spec| spec.op.is_pin());
    if pinned && !policy.bump_pins {
        return Decision::Note(
            policy::Reason::ExactPin,
            "exact pin left unchanged; pass --bump-pins to rewrite ==".to_string(),
        );
    }
    if !req.specifiers.iter().any(|spec| match spec.op {
        Op::GtEq | Op::Gt | Op::Compatible => true,
        Op::Eq | Op::EqLegacy => policy.bump_pins,
        _ => false,
    }) {
        return Decision::Unbounded;
    }
    Decision::Note(
        policy::Reason::LookupFailed,
        "internal error: lookup was required".to_string(),
    )
}

fn make_note(
    name: &str,
    group: &str,
    decoded: &str,
    line: usize,
    reason: policy::Reason,
    detail: &str,
) -> Note {
    Note {
        name: name.to_string(),
        group: group.to_string(),
        requirement: decoded.trim().to_string(),
        reason: reason.as_str().to_string(),
        detail: detail.to_string(),
        line,
    }
}

fn needs_lookup(req: &Requirement, opts: &Options) -> bool {
    if req.is_url || req.specifiers.iter().any(|spec| spec.op == Op::Arbitrary) {
        return false;
    }
    if req.specifiers.iter().any(|spec| spec.op.is_pin()) && !opts.bump_pins {
        return false;
    }
    req.specifiers.iter().any(|spec| match spec.op {
        Op::GtEq | Op::Gt | Op::Compatible => true,
        Op::Eq | Op::EqLegacy => opts.bump_pins,
        _ => false,
    })
}

fn apply_decoded(decoded: &str, edits: &[policy::DecodedEdit]) -> String {
    let mut out = decoded.to_string();
    let mut edits = edits.to_vec();
    edits.sort_by_key(|edit| std::cmp::Reverse(edit.span.start));
    for edit in edits {
        out.replace_range(edit.span, &edit.replacement);
    }
    out
}

fn package_selected(name: &str, opts: &Options) -> bool {
    let normalized = normalize_name(name);
    if !opts.include_packages.is_empty()
        && !opts
            .include_packages
            .iter()
            .any(|package| normalize_name(package) == normalized)
    {
        return false;
    }
    !opts
        .exclude_packages
        .iter()
        .any(|package| normalize_name(package) == normalized)
}

pub(crate) fn group_selected(filter_id: &str, opts: &Options) -> bool {
    if !opts.include_groups.is_empty()
        && !opts
            .include_groups
            .iter()
            .any(|filter| group_match(filter_id, filter))
    {
        return false;
    }
    !opts
        .exclude_groups
        .iter()
        .any(|filter| group_match(filter_id, filter))
}

fn group_match(filter_id: &str, filter: &str) -> bool {
    if filter_id == filter {
        return true;
    }
    if let Some(name) = filter_id.strip_prefix("optional:") {
        return filter == name;
    }
    if let Some(name) = filter_id.strip_prefix("group:") {
        return filter == name;
    }
    if filter_id == "project" {
        return matches!(filter, "dependencies" | "project.dependencies");
    }
    if filter_id == "uv-dev" {
        return matches!(filter, "dev" | "tool.uv.dev-dependencies");
    }
    false
}

fn line_number(text: &str, offset: usize) -> usize {
    let offset = offset.min(text.len());
    text[..offset].bytes().filter(|byte| *byte == b'\n').count() + 1
}

fn guess_name(decoded: &str) -> String {
    decoded
        .split(|c: char| c.is_whitespace() || "[=<>!~@;[".contains(c))
        .find(|part| !part.is_empty())
        .unwrap_or("unknown")
        .to_string()
}

/// Same grid for every package. Golden files are generated against it so the
/// expected bytes do not depend on PyPI.
#[doc(hidden)]
pub fn fixed_test_grid() -> Vec<Release> {
    let mut out = Vec::new();
    for major in 0..20u64 {
        for minor in 0..12u64 {
            if let Some(release) = Release::parse(&format!("{major}.{minor}.0")) {
                out.push(release);
            }
            if let Some(release) = Release::parse(&format!("{major}.{minor}.10")) {
                out.push(release);
            }
        }
        if let Some(release) = Release::parse(&format!("{major}.99.99")) {
            out.push(release);
        }
    }
    if let Some(release) = Release::parse("99.0.0") {
        out.push(release);
    }
    out
}

#[doc(hidden)]
pub struct GridIndex;

impl VersionSource for GridIndex {
    fn releases(&self, _normalized_name: &str) -> Result<Vec<Release>, LookupError> {
        Ok(fixed_test_grid())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bump(text: &str, packages: &[(&str, &[&str])], opts: Options) -> Outcome {
        let mut index = MapIndex::new();
        for (name, versions) in packages {
            index.insert(name, versions);
        }
        bump_str(text, &index, &opts).unwrap()
    }

    fn project(deps: &str) -> String {
        format!("[project]\nname = \"demo\"\nversion = \"0.1.0\"\ndependencies = [\n{deps}\n]\n")
    }

    #[test]
    fn rewrites_only_the_version_token() {
        let text = "\
# comment requests>=9.9.9
[project]
name = \"demo\"
version = \"1.2.3\"
description = \"requests>=1.0.0 should stay\"
dependencies = [
    \"requests>=2.28\", # keep
    \"flask==2.3.2\",
]
";
        let out = bump(
            text,
            &[("requests", &["2.32.3"]), ("flask", &["3.0.0"])],
            Options::default(),
        );
        assert_eq!(out.changes.len(), 1);
        assert!(out.updated.contains("requests>=2.32.3"));
        assert!(out.updated.contains("flask==2.3.2"));
        assert!(out.updated.contains("# comment requests>=9.9.9"));
        assert!(out.updated.contains("version = \"1.2.3\""));
        assert!(out.updated.contains("requests>=1.0.0 should stay"));
        assert!(out.updated.contains("# keep"));
        assert!(preserves_unrelated_bytes(text, &out.updated, &out.edits));
        assert!(out
            .edits
            .iter()
            .all(|edit| is_single_specifier(&edit.original)
                && is_single_specifier(&edit.replacement)));
    }

    #[test]
    fn preserves_crlf_quotes_and_odd_spacing() {
        let text = "[project]\r\nname = 't'\r\nversion = '0.1.0'\r\ndependencies = [\r\n    'pkg  >=  1.0', # c\r\n]\r\n";
        let out = bump(text, &[("pkg", &["2.0.0"])], Options::default());
        assert_eq!(
            out.updated.matches("\r\n").count(),
            text.matches("\r\n").count()
        );
        assert!(out.updated.contains("'pkg  >=  2.0.0'"));
        assert!(out.updated.contains("# c"));
        assert!(preserves_unrelated_bytes(text, &out.updated, &out.edits));
    }

    #[test]
    fn groups_extras_markers_and_include_group() {
        let text = r#"
[project]
name = "demo"
version = "0.1.0"
dependencies = [
    "httpx[http2] >=0.27; python_version >= '3.8'",
    "numpy==1.26.4",
]
optional-dependencies = { dev = ["pytest>=7.0"] }

[dependency-groups]
test = [
    { include-group = "dev" },
    "pytest>=7.0",
]

[tool.uv]
dev-dependencies = ["ruff>=0.4"]

[build-system]
requires = ["hatchling>=1.20,<2"]
"#;
        let out = bump(
            text,
            &[
                ("httpx", &["0.28.1"]),
                ("numpy", &["2.0.0"]),
                ("pytest", &["8.3.2"]),
                ("ruff", &["0.6.9"]),
                ("hatchling", &["1.25.0", "2.0.0"]),
            ],
            Options::default(),
        );
        assert!(out
            .updated
            .contains("httpx[http2] >=0.28.1; python_version >= '3.8'"));
        assert!(out.updated.contains("numpy==1.26.4"));
        assert!(out.updated.contains("{ include-group = \"dev\" }"));
        assert!(out.updated.contains("hatchling>=1.25.0,<2"));
        assert!(out.notes.iter().any(|note| note.reason == "exact_pin"));
        assert!(preserves_unrelated_bytes(text, &out.updated, &out.edits));
        assert!(out.edits.iter().all(|edit| {
            is_single_specifier(&edit.original) && is_single_specifier(&edit.replacement)
        }));
    }

    #[test]
    fn include_and_exclude_filters() {
        let text = project("    \"requests>=1\",\n    \"flask>=1\",\n");
        let mut opts = Options::default();
        opts.exclude_packages.insert("flask".to_string());
        let out = bump(&text, &[("requests", &["2"]), ("flask", &["3"])], opts);
        assert!(out.updated.contains("requests>=2"));
        assert!(out.updated.contains("flask>=1"));

        let mut opts = Options::default();
        opts.include_packages.insert("Flask".to_string());
        let out = bump(&text, &[("requests", &["2"]), ("flask", &["3"])], opts);
        assert!(out.updated.contains("requests>=1"));
        assert!(out.updated.contains("flask>=3"));
    }

    #[test]
    fn group_filter_and_dev_alias() {
        let text = "\
[project]
name = \"demo\"
version = \"0\"
dependencies = [\"requests>=1\"]

[dependency-groups]
dev = [\"pytest>=7\"]

[tool.uv]
dev-dependencies = [\"ruff>=0.4\"]
";
        let mut opts = Options::default();
        opts.include_groups.insert("group:dev".to_string());
        let out = bump(
            text,
            &[("requests", &["2"]), ("pytest", &["8"]), ("ruff", &["0.6"])],
            opts,
        );
        assert!(out.updated.contains("requests>=1"));
        assert!(out.updated.contains("pytest>=8"));
        assert!(out.updated.contains("ruff>=0.4"));

        let mut opts = Options::default();
        opts.exclude_groups.insert("uv-dev".to_string());
        let out = bump(
            text,
            &[("requests", &["2"]), ("pytest", &["8"]), ("ruff", &["0.6"])],
            opts,
        );
        assert!(out.updated.contains("requests>=2"));
        assert!(out.updated.contains("pytest>=8"));
        assert!(out.updated.contains("ruff>=0.4"));

        let mut opts = Options::default();
        opts.include_groups.insert("dev".to_string());
        let out = bump(
            text,
            &[("requests", &["2"]), ("pytest", &["8"]), ("ruff", &["0.6"])],
            opts,
        );
        assert!(out.updated.contains("requests>=1"));
        assert!(out.updated.contains("pytest>=8"));
        assert!(out.updated.contains("ruff>=0.6"));
    }

    #[test]
    fn escaped_marker_and_greater_than() {
        let text = "[project]\nname=\"t\"\nversion=\"0\"\ndependencies=[\"pkg>=1.0; python_version == \\\"3.11\\\"\", \"other>1\"]\n";
        let out = bump(
            text,
            &[("pkg", &["2.0.0"]), ("other", &["3.1.0"])],
            Options::default(),
        );
        assert!(out
            .updated
            .contains("pkg>=2.0.0; python_version == \\\"3.11\\\""));
        assert!(out.updated.contains("other>=3.1.0"));
        assert!(preserves_unrelated_bytes(text, &out.updated, &out.edits));
        assert!(out.edits.iter().all(|edit| {
            is_single_specifier(&edit.original) && is_single_specifier(&edit.replacement)
        }));
    }

    #[test]
    fn multiline_string_and_dotted_key() {
        let text =
            "[project]\nname='t'\nversion='0'\ndependencies = [\"\"\"\nrequests>=1.0\n\"\"\"]\n";
        let out = bump(text, &[("requests", &["2.2.0"])], Options::default());
        assert!(out.updated.contains("requests>=2.2.0"));
        assert!(out.updated.starts_with("[project]\n"));
        assert!(preserves_unrelated_bytes(text, &out.updated, &out.edits));

        let dotted = "project.name='t'\nproject.version='0'\nproject.dependencies=['idna>=2']\n";
        let out = bump(dotted, &[("idna", &["3.10"])], Options::default());
        assert!(out.updated.contains("idna>=3.10") || out.updated.contains("'idna>=3.10'"));
        assert!(preserves_unrelated_bytes(dotted, &out.updated, &out.edits));
    }

    #[test]
    fn malformed_toml_is_an_error() {
        let err = bump_str("[\n", &MapIndex::new(), &Options::default()).unwrap_err();
        assert!(err.to_string().contains("TOML"));
    }

    #[test]
    fn comment_only_file_is_unchanged() {
        let text = "# nothing here\n";
        let out = bump_str(text, &MapIndex::new(), &Options::default()).unwrap();
        assert_eq!(out.updated, text);
        assert!(out.changes.is_empty());
    }

    #[test]
    fn not_found_is_a_note() {
        let text = project("    \"missing>=1\",\n");
        let out = bump(&text, &[], Options::default());
        assert_eq!(out.updated, text);
        assert_eq!(out.notes[0].reason, "not_found");
    }
}
