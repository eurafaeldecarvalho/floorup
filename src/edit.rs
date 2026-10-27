//! Find dependency strings and splice new specifier text into the original bytes.
//!
//! `toml_edit` is used only to locate string values. The file is never
//! re-serialized, so comments, spacing, quotes, and key order survive.

use crate::policy::DecodedEdit;
use crate::Error;
use std::ops::Range;
use toml_edit::{Array, ImDocument, Item};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Located {
    pub filter_id: String,
    pub group_label: String,
    /// Decoded TOML string (escapes already interpreted).
    pub decoded: String,
    /// Byte span of the TOML value, including quotes, in the original file.
    pub value_span: Range<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEdit {
    pub span: Range<usize>,
    pub original: String,
    pub replacement: String,
}

pub fn locate(text: &str) -> Result<Vec<Located>, Error> {
    // `ImDocument` keeps byte spans. `DocumentMut` materializes them into
    // owned strings, which would force a full rewrite.
    let doc = ImDocument::parse(text)
        .map_err(|err| Error::Message(format!("could not parse TOML: {err}")))?;
    let mut out = Vec::new();
    if let Some(project) = doc.get("project") {
        if let Some(deps) = project.get("dependencies").and_then(Item::as_array) {
            push_array(&mut out, text, deps, "project", "project.dependencies");
        }
        if let Some(optional) = project.get("optional-dependencies") {
            for (name, array) in arrays_of(optional) {
                push_array(
                    &mut out,
                    text,
                    array,
                    &format!("optional:{name}"),
                    &format!("project.optional-dependencies.{name}"),
                );
            }
        }
    }
    if let Some(groups) = doc.get("dependency-groups") {
        for (name, array) in arrays_of(groups) {
            push_array(
                &mut out,
                text,
                array,
                &format!("group:{name}"),
                &format!("dependency-groups.{name}"),
            );
        }
    }
    if let Some(dev) = doc
        .get("tool")
        .and_then(|tool| tool.get("uv"))
        .and_then(|uv| uv.get("dev-dependencies"))
        .and_then(Item::as_array)
    {
        push_array(&mut out, text, dev, "uv-dev", "tool.uv.dev-dependencies");
    }
    if let Some(requires) = doc
        .get("build-system")
        .and_then(|build| build.get("requires"))
        .and_then(Item::as_array)
    {
        push_array(
            &mut out,
            text,
            requires,
            "build-system",
            "build-system.requires",
        );
    }
    Ok(out)
}

fn arrays_of(item: &Item) -> Vec<(String, &Array)> {
    let mut out = Vec::new();
    if let Some(table) = item.as_table() {
        for (key, value) in table.iter() {
            if let Some(array) = value.as_array() {
                out.push((key.to_string(), array));
            }
        }
    } else if let Some(value) = item.as_value() {
        if let Some(table) = value.as_inline_table() {
            for (key, value) in table.iter() {
                if let Some(array) = value.as_array() {
                    out.push((key.to_string(), array));
                }
            }
        }
    }
    out
}

fn push_array(
    out: &mut Vec<Located>,
    text: &str,
    array: &Array,
    filter_id: &str,
    group_label: &str,
) {
    for value in array.iter() {
        let Some(decoded) = value.as_str() else {
            // Inline tables such as `{ include-group = "dev" }` are not
            // requirements. Leave them byte-for-byte alone.
            continue;
        };
        let Some(value_span) = value.span() else {
            continue;
        };
        if text.get(value_span.clone()).is_none() {
            continue;
        }
        out.push(Located {
            filter_id: filter_id.to_string(),
            group_label: group_label.to_string(),
            decoded: decoded.to_string(),
            value_span,
        });
    }
}

/// Map a specifier edit inside a decoded requirement onto the original file.
pub fn map_edit(text: &str, located: &Located, edit: &DecodedEdit) -> Option<FileEdit> {
    let raw = text.get(located.value_span.clone())?;
    let interior = string_interior(raw)?;
    let raw_interior = raw.get(interior.clone())?;
    let spec_text = located.decoded.get(edit.span.clone())?;
    let file_start = located.value_span.start + interior.start;

    if let Some(base) = unique_find(raw_interior, &located.decoded) {
        let start = file_start + base + edit.span.start;
        let end = start + spec_text.len();
        if text.get(start..end) == Some(spec_text) {
            return Some(FileEdit {
                span: start..end,
                original: spec_text.to_string(),
                replacement: edit.replacement.clone(),
            });
        }
    }

    let nth = located.decoded[..edit.span.start]
        .matches(spec_text)
        .count();
    let pos = nth_find(raw_interior, spec_text, nth)?;
    let start = file_start + pos;
    let end = start + spec_text.len();
    if text.get(start..end) != Some(spec_text) {
        return None;
    }
    Some(FileEdit {
        span: start..end,
        original: spec_text.to_string(),
        replacement: edit.replacement.clone(),
    })
}

pub fn apply_edits(text: &str, edits: &[FileEdit]) -> Result<String, Error> {
    let mut ordered = edits.to_vec();
    ordered.sort_by(|a, b| {
        b.span
            .start
            .cmp(&a.span.start)
            .then(b.span.end.cmp(&a.span.end))
    });
    for pair in ordered.windows(2) {
        if pair[1].span.end > pair[0].span.start {
            return Err(Error::Message(
                "refusing to write overlapping edits".to_string(),
            ));
        }
    }
    let mut out = text.to_string();
    for edit in &ordered {
        if !out.is_char_boundary(edit.span.start) || !out.is_char_boundary(edit.span.end) {
            return Err(Error::Message(
                "edit span was not on a character boundary".to_string(),
            ));
        }
        if out
            .get(edit.span.clone())
            .is_none_or(|slice| slice != edit.original)
        {
            return Err(Error::Message(
                "edit did not match the original specifier text".to_string(),
            ));
        }
        out.replace_range(edit.span.clone(), &edit.replacement);
    }
    Ok(out)
}

/// Byte slices with the edited spans removed. Equal residuals mean every byte
/// outside a specifier edit is unchanged, including across length changes.
pub fn preserves_unrelated_bytes(original: &str, updated: &str, edits: &[FileEdit]) -> bool {
    let Ok(rebuilt) = apply_edits(original, edits) else {
        return false;
    };
    if rebuilt != updated {
        return false;
    }
    let mut ordered = edits.to_vec();
    ordered.sort_by_key(|edit| edit.span.start);
    let mut old_spans = Vec::new();
    let mut new_spans = Vec::new();
    let mut shift: isize = 0;
    for edit in &ordered {
        old_spans.push(edit.span.clone());
        let start = (edit.span.start as isize + shift) as usize;
        let end = start + edit.replacement.len();
        new_spans.push(start..end);
        shift += edit.replacement.len() as isize - (edit.span.end - edit.span.start) as isize;
    }
    residual(original, &old_spans) == residual(updated, &new_spans)
}

fn residual(text: &str, spans: &[Range<usize>]) -> String {
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0;
    for span in spans {
        if span.start > text.len() || span.end > text.len() || span.start < cursor {
            return String::new();
        }
        out.push_str(&text[cursor..span.start]);
        cursor = span.end;
    }
    out.push_str(&text[cursor..]);
    out
}

fn string_interior(raw: &str) -> Option<Range<usize>> {
    for (open, close) in [
        ("'''", "'''"),
        ("\"\"\"", "\"\"\""),
        ("'", "'"),
        ("\"", "\""),
    ] {
        if let Some(rest) = raw.strip_prefix(open) {
            if let Some(inner) = rest.strip_suffix(close) {
                if raw.len() >= open.len() + close.len() {
                    return Some(open.len()..open.len() + inner.len());
                }
            }
        }
    }
    None
}

fn unique_find(haystack: &str, needle: &str) -> Option<usize> {
    let start = haystack.find(needle)?;
    if haystack[start + needle.len()..].contains(needle) {
        None
    } else {
        Some(start)
    }
}

fn nth_find(haystack: &str, needle: &str, nth: usize) -> Option<usize> {
    let mut start = 0;
    let mut seen = 0;
    while let Some(pos) = haystack[start..].find(needle) {
        let abs = start + pos;
        if seen == nth {
            return Some(abs);
        }
        seen += 1;
        start = abs + needle.len();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spans_point_at_requirement_strings() {
        let text = "\
[project]
dependencies = [
    \"requests>=1.0\", # comment
    'flask>=2.0',
]

[project.optional-dependencies]
dev = [\"pytest>=7\"]

[dependency-groups]
lint = [
    { include-group = \"dev\" },
    \"ruff>=0.4\",
]

[tool.uv]
dev-dependencies = [\"mypy>=1\"]

[build-system]
requires = [\"hatchling>=1.0\"]
";
        let located = locate(text).unwrap();
        let decoded: Vec<_> = located.iter().map(|item| item.decoded.as_str()).collect();
        assert_eq!(
            decoded,
            vec![
                "requests>=1.0",
                "flask>=2.0",
                "pytest>=7",
                "ruff>=0.4",
                "mypy>=1",
                "hatchling>=1.0",
            ]
        );
        for item in &located {
            let raw = &text[item.value_span.clone()];
            assert!(raw.contains(&item.decoded), "{raw}");
        }
        assert!(located.iter().any(|item| item.filter_id == "optional:dev"));
        assert!(located.iter().any(|item| item.filter_id == "group:lint"));
        assert!(located.iter().any(|item| item.filter_id == "uv-dev"));
        assert!(located.iter().any(|item| item.filter_id == "build-system"));
    }

    #[test]
    fn inline_table_of_extras_is_visible() {
        let text = "[project]\noptional-dependencies = { dev = [\"pytest>=7\"] }\n";
        let located = locate(text).unwrap();
        assert_eq!(located.len(), 1);
        assert_eq!(located[0].decoded, "pytest>=7");
        assert_eq!(located[0].filter_id, "optional:dev");
    }
}
