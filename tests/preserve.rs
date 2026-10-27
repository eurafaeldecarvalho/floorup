//! Property tests: random but valid pyproject files only change specifier bytes.

use floorup::{bump_str, is_single_specifier, preserves_unrelated_bytes, MapIndex, Options};
use proptest::prelude::*;
use proptest::test_runner::{Config as ProptestConfig, FileFailurePersistence};
use std::collections::BTreeSet;

fn arb_name() -> impl Strategy<Value = String> {
    "[a-z][a-z0-9]{0,7}".prop_map(|name| name)
}

fn arb_version() -> impl Strategy<Value = String> {
    (0u64..9, 0u64..9, 0u64..9).prop_map(|(major, minor, patch)| format!("{major}.{minor}.{patch}"))
}

fn arb_comment() -> impl Strategy<Value = String> {
    "[A-Za-z0-9 .,_-]{0,24}"
}

#[derive(Debug, Clone)]
struct Dep {
    name: String,
    op: String,
    version: String,
    spaces: usize,
    marker: bool,
    extra: bool,
}

fn arb_dep() -> impl Strategy<Value = Dep> {
    (
        arb_name(),
        prop_oneof![" >=", ">=", "~=", "==", ">"],
        arb_version(),
        0usize..3,
        any::<bool>(),
        any::<bool>(),
    )
        .prop_map(|(name, op, version, spaces, marker, extra)| Dep {
            name,
            op,
            version,
            spaces,
            marker,
            extra,
        })
}

fn render(head: &str, deps: &[Dep], tail: &str) -> String {
    let mut body = String::new();
    body.push_str("# ");
    body.push_str(head);
    body.push_str("\n[project]\nname = \"demo\"\nversion = \"0.1.0\"\ndescription = \"requests>=9.9.9\"\ndependencies = [\n");
    for dep in deps {
        let extra = if dep.extra { "[extra]" } else { "" };
        let marker = if dep.marker {
            "; python_version>='3.8'"
        } else {
            ""
        };
        let ws = " ".repeat(dep.spaces);
        body.push_str(&format!(
            "    \"{}{extra}{}{ws}{}{marker}\", # tail\n",
            dep.name, dep.op, dep.version
        ));
    }
    body.push_str("]\n[tool.other]\nnote = \"\"\"\n");
    body.push_str(tail);
    body.push_str("\n\"\"\"\n");
    body
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 80,
        failure_persistence: Some(Box::new(FileFailurePersistence::Off)),
        ..ProptestConfig::default()
    })]

    #[test]
    fn non_dependency_bytes_stay_put(
        head in "[A-Za-z0-9 .,_-]{0,30}",
        deps in proptest::collection::vec(arb_dep(), 1..6),
        tail in arb_comment(),
    ) {
        let mut seen = BTreeSet::new();
        let deps: Vec<_> = deps.into_iter().filter(|dep| seen.insert(dep.name.clone())).collect();
        prop_assume!(!deps.is_empty());
        let text = render(&head, &deps, &tail);
        let mut index = MapIndex::new();
        for dep in &deps {
            index.insert(&dep.name, &["9.9.9", "1.0.0"]);
        }
        let outcome = bump_str(&text, &index, &Options::default()).expect("bump");
        prop_assert!(preserves_unrelated_bytes(&text, &outcome.updated, &outcome.edits));
        for edit in &outcome.edits {
            prop_assert!(is_single_specifier(&edit.original), "{:?}", edit.original);
            prop_assert!(is_single_specifier(&edit.replacement), "{:?}", edit.replacement);
        }
        prop_assert!(outcome.updated.contains("requests>=9.9.9"));
        prop_assert!(outcome.updated.contains(&head));
        prop_assert!(outcome.updated.contains(&tail));
    }
}
