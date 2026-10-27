//! Unified diff of two texts. Used by `--dry-run` and the JSON report.

pub fn unified(path: &str, before: &str, after: &str) -> String {
    if before == after {
        return String::new();
    }
    let diff = similar::TextDiff::from_lines(before, after);
    diff.unified_diff()
        .context_radius(3)
        .header(path, path)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shows_only_the_changed_line() {
        let before = "[project]\ndependencies = [\"requests>=1.0\"]\n";
        let after = "[project]\ndependencies = [\"requests>=2.0\"]\n";
        let diff = unified("pyproject.toml", before, after);
        assert!(diff.contains("-dependencies = [\"requests>=1.0\"]"));
        assert!(diff.contains("+dependencies = [\"requests>=2.0\"]"));
        assert!(diff.contains("pyproject.toml"));
    }

    #[test]
    fn empty_when_equal() {
        assert!(unified("a", "same\n", "same\n").is_empty());
    }
}
