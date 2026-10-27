//! Live PyPI check. Skipped unless `FLOORUP_PYPI_LIVE=1`.
//!
//! ```text
//! FLOORUP_PYPI_LIVE=1 cargo test --test live -- --nocapture
//! ```

use floorup::{bump_str, Options, PypiConfig, PypiIndex, VersionSource};

#[test]
fn live_requests_lower_bound_moves_to_a_real_release() {
    if std::env::var("FLOORUP_PYPI_LIVE").ok().as_deref() != Some("1") {
        eprintln!("skipping live PyPI test; set FLOORUP_PYPI_LIVE=1 to run it");
        return;
    }
    let index = PypiIndex::new(PypiConfig {
        cache_dir: None,
        timeout: std::time::Duration::from_secs(30),
        jobs: 4,
        ..PypiConfig::default()
    })
    .unwrap();
    let text = "[project]\nname = \"t\"\nversion = \"0.1.0\"\ndependencies = [\"requests>=2.0\"]\n";
    let outcome = bump_str(text, &index, &Options::default()).unwrap();
    assert_eq!(outcome.changes.len(), 1, "{:?}", outcome.notes);
    let after = &outcome.changes[0].after;
    assert!(after.starts_with("requests>="), "{after}");
    assert_ne!(after, "requests>=2.0");
    let version = after.trim_start_matches("requests>=");
    let releases = index.releases("requests").unwrap();
    assert!(
        releases.iter().any(|rel| rel.version_text == version),
        "{version} was not in the PyPI release list"
    );
}
