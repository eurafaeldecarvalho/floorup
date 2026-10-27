//! Edge cases the golden files do not exercise.

use floorup::{
    bump_str, is_single_specifier, preserves_unrelated_bytes, MapIndex, Options, Release,
};
use std::collections::BTreeSet;

fn project(deps: &str) -> String {
    format!("[project]\nname = \"demo\"\nversion = \"0.1.0\"\ndependencies = [\n{deps}\n]\n")
}

fn bump(text: &str, packages: &[(&str, &[&str])], opts: Options) -> floorup::Outcome {
    let mut index = MapIndex::new();
    for (name, versions) in packages {
        index.insert(name, versions);
    }
    bump_str(text, &index, &opts).unwrap_or_else(|err| panic!("bump failed: {err}"))
}

fn assert_preserved(before: &str, outcome: &floorup::Outcome) {
    assert!(
        preserves_unrelated_bytes(before, &outcome.updated, &outcome.edits),
        "unrelated bytes changed\nbefore: {before:?}\nafter: {:?}\nedits: {:?}",
        outcome.updated,
        outcome.edits
    );
    for edit in &outcome.edits {
        assert!(is_single_specifier(&edit.original), "{:?}", edit.original);
        assert!(
            is_single_specifier(&edit.replacement),
            "{:?}",
            edit.replacement
        );
    }
}

#[test]
fn second_run_changes_nothing() {
    let text = project(
        "    \"requests>=1.0\",\n    \"flask>1\",\n    \"django~=4.2\",\n    \"idna>=2,<3,!=2.8\",\n",
    );
    let versions = &[
        ("requests", &["2.32.3"][..]),
        ("flask", &["2.0.0"][..]),
        ("django", &["4.2.16", "5.0.1"][..]),
        ("idna", &["2.8", "2.10", "3.10"][..]),
    ];
    let once = bump(&text, versions, Options::default());
    assert!(!once.changes.is_empty());
    assert_preserved(&text, &once);
    let twice = bump(&once.updated, versions, Options::default());
    assert_eq!(twice.updated, once.updated);
    assert!(twice.changes.is_empty(), "{:?}", twice.changes);
}

#[test]
fn names_extras_markers_urls_and_duplicates() {
    let text = "\
[project]
name = 'demo'
version = '0'
dependencies = [
    \"Foo_Bar.Baz>=1.0\",
    'pkg[extra.one, extra-two]>=1; marker == \"a;b >=1\"',
    \"pkg>=1.0\",
    \"repo @ git+https://example.com/repo.git\",
    \"local[extra]@file:///tmp/local\",
    \"pathdep @ ./vendor/pathdep\",
    \"pkg @ git+https://example.com/pkg.git ; extra == 'dev'\",
]
optional-dependencies.dev = [\"pytest>=7\"]
";
    let out = bump(
        text,
        &[
            ("foo-bar-baz", &["1.2.0"]),
            ("pkg", &["3.0.0"]),
            ("pytest", &["8.0.0"]),
            ("repo", &["9.0.0"]),
            ("local", &["9.0.0"]),
            ("pathdep", &["9.0.0"]),
        ],
        Options::default(),
    );
    assert!(
        out.updated.contains("Foo_Bar.Baz>=1.2.0"),
        "{}",
        out.updated
    );
    assert!(
        out.updated
            .contains("pkg[extra.one, extra-two]>=3.0.0; marker == \"a;b >=1\""),
        "{}",
        out.updated
    );
    assert!(out.updated.contains("\"pkg>=3.0.0\""), "{}", out.updated);
    assert!(out
        .updated
        .contains("repo @ git+https://example.com/repo.git"));
    assert!(out.updated.contains("local[extra]@file:///tmp/local"));
    assert!(out.updated.contains("pathdep @ ./vendor/pathdep"));
    assert!(out
        .notes
        .iter()
        .any(|note| note.reason == "direct_reference"));
    assert_preserved(text, &out);
    let again = bump(
        &out.updated,
        &[
            ("pkg", &["3.0.0"]),
            ("foo-bar-baz", &["1.2.0"]),
            ("pytest", &["8.0.0"]),
        ],
        Options::default(),
    );
    assert_eq!(again.updated, out.updated);
}

#[test]
fn formatting_oddities_stay_byte_stable() {
    let text = "\u{feff}# comment pkg>=9\r\n[project]\r\nname='t'\r\nversion='0'\r\ndependencies = [\r\n\t'requests>=1.0', # keep >=1.0\r\n    \"\"\"\r\nflask>=1\r\n\"\"\",\r\n    '''\r\nidna>=1\r\n''',\r\n]\r\n";
    let out = bump(
        text,
        &[
            ("requests", &["2.0.0"]),
            ("flask", &["3.0.0"]),
            ("idna", &["3.8"]),
        ],
        Options::default(),
    );
    assert!(out.updated.starts_with('\u{feff}'));
    assert!(out.updated.contains("'requests>=2.0.0'"));
    assert!(out.updated.contains("# keep >=1.0"));
    assert!(out.updated.contains("# comment pkg>=9"));
    assert!(out.updated.contains("flask>=3.0.0"));
    assert!(out.updated.contains("idna>=3.8"));
    assert_eq!(
        out.updated.matches("\r\n").count(),
        text.matches("\r\n").count()
    );
    assert_preserved(text, &out);

    let tabs = "[project]\nname='t'\nversion='0'\ndependencies=['pkg>=\t1.0']\n";
    let out = bump(tabs, &[("pkg", &["2.0.0"])], Options::default());
    assert!(out.updated.contains("pkg>=\t2.0.0"), "{}", out.updated);
    assert_preserved(tabs, &out);

    let inline = "project={name='t',version='0',dependencies=['pkg>=1']}\nbuild-system={requires=['hatchling>=1.20,<2'],build-backend='hatchling.build'}\ndependency-groups={dev=['pytest>=7']}\n";
    let out = bump(
        inline,
        &[
            ("pkg", &["2.0"]),
            ("hatchling", &["1.25.0", "2.0.0"]),
            ("pytest", &["8"]),
        ],
        Options::default(),
    );
    assert!(out.updated.contains("pkg>=2.0"), "{}", out.updated);
    assert!(
        out.updated.contains("hatchling>=1.25.0,<2"),
        "{}",
        out.updated
    );
    assert!(out.updated.contains("pytest>=8"), "{}", out.updated);
    assert!(out.updated.contains("build-backend='hatchling.build'"));
    assert_preserved(inline, &out);
}

#[test]
fn empty_malformed_dynamic_and_missing_packages() {
    let empty = bump_str("", &MapIndex::new(), &Options::default()).unwrap();
    assert_eq!(empty.updated, "");
    assert!(empty.changes.is_empty());

    let bom = bump_str("\u{feff}", &MapIndex::new(), &Options::default()).unwrap();
    assert_eq!(bom.updated, "\u{feff}");

    let err = bump_str("[\n", &MapIndex::new(), &Options::default()).unwrap_err();
    assert!(err.to_string().contains("TOML"), "{err}");
    let err = bump_str("\u{feff}[\n", &MapIndex::new(), &Options::default()).unwrap_err();
    assert!(err.to_string().contains("TOML"), "{err}");

    let dynamic_only = "\
[project]
name = 't'
version = '0'
dynamic = ['dependencies']
";
    let out = bump(dynamic_only, &[("requests", &["2"])], Options::default());
    assert_eq!(out.updated, dynamic_only);

    // Both keys is invalid PEP 621. The static array is still a dependency list.
    let both = "\
[project]
name = 't'
version = '0'
dynamic = ['dependencies']
dependencies = ['requests>=1']
";
    let out = bump(both, &[("requests", &["2.0.0"])], Options::default());
    assert!(out.updated.contains("requests>=2.0.0"));
    assert!(out.updated.contains("dynamic = ['dependencies']"));
    assert_preserved(both, &out);

    let missing = project("    \"not-a-real-distribution>=1\",\n");
    let out = bump(&missing, &[], Options::default());
    assert_eq!(out.updated, missing);
    assert_eq!(out.notes[0].reason, "not_found");
}

#[test]
fn yanked_prerelease_only_and_partial_yank() {
    let mut index = MapIndex::new();
    let mut yanked = Release::parse("2.0.0").unwrap();
    yanked.yanked = true;
    let mut partial = Release::parse("1.5.0").unwrap();
    partial.yanked = false;
    index.insert_releases(
        "pkg",
        vec![Release::parse("1.0.0").unwrap(), partial, yanked],
    );
    let text = project("    \"pkg>=1\",\n");
    let out = bump_str(&text, &index, &Options::default()).unwrap();
    assert!(out.updated.contains("pkg>=1.5.0"), "{}", out.updated);

    let mut only_yanked = MapIndex::new();
    let mut rel = Release::parse("1.0.0").unwrap();
    rel.yanked = true;
    only_yanked.insert_releases("pkg", vec![rel]);
    let out = bump_str(&text, &only_yanked, &Options::default()).unwrap();
    assert_eq!(out.updated, text);
    assert_eq!(out.notes[0].reason, "no_eligible_release");

    let mut pres = MapIndex::new();
    pres.insert("pkg", &["1.0.0rc1", "1.0.0a1"]);
    let pre_only = project("    \"pkg>=0.9\",\n");
    let out = bump_str(&pre_only, &pres, &Options::default()).unwrap();
    assert_eq!(out.updated, pre_only);
    assert_eq!(out.notes[0].reason, "no_eligible_release");
    let opts = Options {
        allow_pre: true,
        ..Options::default()
    };
    let out = bump_str(&pre_only, &pres, &opts).unwrap();
    assert!(out.updated.contains("pkg>=1.0.0rc1"), "{}", out.updated);
    let again = bump_str(&out.updated, &pres, &opts).unwrap();
    assert_eq!(again.updated, out.updated);
}

#[test]
fn escaped_quotes_and_duplicate_specifier_text_map_to_the_operator() {
    let text =
        "[project]\nname='t'\nversion='0'\ndependencies=[\"pkg>=1; extra == \\\">=1\\\"\"]\n";
    let out = bump(text, &[("pkg", &["2.0.0"])], Options::default());
    assert_eq!(
        out.updated,
        "[project]\nname='t'\nversion='0'\ndependencies=[\"pkg>=2.0.0; extra == \\\">=1\\\"\"]\n"
    );
    assert_preserved(text, &out);
}

#[test]
fn filters_normalize_names() {
    let text = project("    \"Typing_Extensions>=4\",\n    \"flask>=1\",\n");
    let opts = Options {
        include_packages: BTreeSet::from(["typing.extensions".to_string()]),
        ..Options::default()
    };
    let out = bump(
        &text,
        &[("typing-extensions", &["4.12.2"]), ("flask", &["3"])],
        opts,
    );
    assert!(out.updated.contains("Typing_Extensions>=4.12.2"));
    assert!(out.updated.contains("flask>=1"));
    assert_preserved(&text, &out);
}
