//! Which release to write into a requirement, and how to spell it.
//!
//! Ceilings (`<`, `<=`, `!=`, and `~=` unless expanded) are never widened.
//! Exact pins stay put unless the caller opts in. `>` is rewritten to `>=`
//! only when the chosen release is strictly newer, so the release we just
//! selected is actually allowed by the new specifier.

use crate::parse::{Op, Requirement, Specifier};
use crate::pypi::Release;
use pep440_rs::{Version, VersionSpecifier};
use std::cmp::Ordering;
use std::ops::Range;
use std::str::FromStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    ExactPin,
    DirectReference,
    ArbitraryEquality,
    NoEligibleRelease,
    NotFound,
    Unparseable,
    CouldNotEdit,
    LookupFailed,
}

impl Reason {
    pub fn as_str(self) -> &'static str {
        match self {
            Reason::ExactPin => "exact_pin",
            Reason::DirectReference => "direct_reference",
            Reason::ArbitraryEquality => "arbitrary_equality",
            Reason::NoEligibleRelease => "no_eligible_release",
            Reason::NotFound => "not_found",
            Reason::Unparseable => "unparseable",
            Reason::CouldNotEdit => "could_not_edit",
            Reason::LookupFailed => "lookup_failed",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Specifier slices (relative to the decoded requirement) and their replacements.
    Bump(Vec<DecodedEdit>),
    Current,
    Unbounded,
    Note(Reason, String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedEdit {
    pub span: Range<usize>,
    pub replacement: String,
}

#[derive(Debug, Clone, Copy)]
pub struct Policy {
    pub allow_pre: bool,
    pub bump_pins: bool,
    pub expand_compatible: bool,
}

pub fn decide(req: &Requirement, releases: Option<Lookup>, policy: Policy) -> Decision {
    if let Some(decision) = classify(req, policy) {
        return decision;
    }

    let releases = match releases {
        Some(Lookup::Found(list)) => list,
        Some(Lookup::NotFound) => {
            return Decision::Note(
                Reason::NotFound,
                "package was not found on the index".to_string(),
            );
        }
        Some(Lookup::Failed(message)) => return Decision::Note(Reason::LookupFailed, message),
        None => {
            return Decision::Note(
                Reason::LookupFailed,
                "internal error: missing version lookup".to_string(),
            );
        }
    };

    let allow_pre = policy.allow_pre || req.specifiers.iter().any(|s| s.version.any_prerelease());
    let Some(target) = select_target(req, &releases, policy, allow_pre, true) else {
        // Nothing strictly newer. If relaxing `>` to `>=` finds a release,
        // the bound is already at the newest eligible version.
        if select_target(req, &releases, policy, allow_pre, false).is_some() {
            return Decision::Current;
        }
        return Decision::Note(
            Reason::NoEligibleRelease,
            "no release satisfies the existing constraints".to_string(),
        );
    };

    let mut edits = Vec::new();
    for spec in &req.specifiers {
        if let Some(replacement) = rewrite_specifier(spec, target, policy) {
            let span = req.span_in_decoded(spec.span());
            edits.push(DecodedEdit { span, replacement });
        }
    }
    if edits.is_empty() {
        Decision::Current
    } else {
        Decision::Bump(edits)
    }
}

/// A lookup result already classified for the policy.
#[derive(Debug, Clone)]
pub enum Lookup {
    Found(Vec<Release>),
    NotFound,
    Failed(String),
}

/// Decisions that do not need a version list: skips, pins, and bare names.
///
/// `None` means a release list is required.
pub(crate) fn classify(req: &Requirement, policy: Policy) -> Option<Decision> {
    if req.is_url {
        return Some(Decision::Note(
            Reason::DirectReference,
            "direct URL, path, or VCS requirement".to_string(),
        ));
    }
    if req.specifiers.iter().any(|s| s.op == Op::Arbitrary) {
        return Some(Decision::Note(
            Reason::ArbitraryEquality,
            "=== is an arbitrary string match and is not rewritten".to_string(),
        ));
    }
    if let Err(message) = validate_specifiers(req) {
        return Some(Decision::Note(Reason::Unparseable, message));
    }
    if req.specifiers.iter().any(is_exact_pin) && !policy.bump_pins {
        return Some(Decision::Note(
            Reason::ExactPin,
            "exact pin left unchanged; pass --bump-pins to rewrite ==".to_string(),
        ));
    }
    if !req.specifiers.iter().any(|s| is_rewritten_floor(s, policy)) {
        if req.specifiers.iter().any(|s| s.op.is_pin()) {
            return Some(Decision::Note(
                Reason::ExactPin,
                "wildcard equality (==1.*) is left unchanged".to_string(),
            ));
        }
        return Some(Decision::Unbounded);
    }
    None
}

fn is_exact_pin(spec: &Specifier) -> bool {
    spec.op.is_pin() && !spec.star
}

fn validate_specifiers(req: &Requirement) -> Result<(), String> {
    for spec in &req.specifiers {
        if spec.op == Op::Arbitrary {
            continue;
        }
        let text = format!("{}{}", spec.op.pep440(), spec.version_text);
        if let Err(e) = VersionSpecifier::from_str(&text) {
            return Err(format!("invalid specifier {text}: {e}"));
        }
    }
    Ok(())
}

fn is_rewritten_floor(spec: &Specifier, policy: Policy) -> bool {
    match spec.op {
        Op::GtEq | Op::Gt | Op::Compatible => true,
        // `==1.*` is a prefix constraint, not an exact pin.
        Op::Eq | Op::EqLegacy => policy.bump_pins && !spec.star,
        _ => false,
    }
}

fn select_target<'a>(
    req: &Requirement,
    releases: &'a [Release],
    policy: Policy,
    allow_pre: bool,
    strict_greater: bool,
) -> Option<&'a Release> {
    let mut best: Option<&Release> = None;
    for rel in releases {
        if rel.yanked || rel.version.is_local() {
            continue;
        }
        if !rel.version.is_stable() && !allow_pre {
            continue;
        }
        if !within_ceilings(req, &rel.version, policy, allow_pre) {
            continue;
        }
        if !meets_floors(req, &rel.version, policy, strict_greater) {
            continue;
        }
        if !compatible_spelling_admits(req, rel, policy) {
            continue;
        }
        best = Some(match best {
            None => rel,
            Some(cur) => pick_newer(cur, rel),
        });
    }
    best
}

fn pick_newer<'a>(a: &'a Release, b: &'a Release) -> &'a Release {
    match a.version.cmp(&b.version) {
        Ordering::Less => b,
        Ordering::Greater => a,
        Ordering::Equal => {
            if b.version_text.len() > a.version_text.len() {
                b
            } else {
                a
            }
        }
    }
}

fn within_ceilings(req: &Requirement, cand: &Version, policy: Policy, allow_pre: bool) -> bool {
    for spec in &req.specifiers {
        let ok = match spec.op {
            Op::Lt => satisfies_lt(&spec.version, cand),
            Op::LtEq => cmp_local(cand, &spec.version) != Ordering::Greater,
            Op::NotEq => not_equal(spec, cand),
            Op::Eq | Op::EqLegacy if spec.star => prefix_match(&spec.version, cand),
            Op::Compatible if !policy.expand_compatible => compatible(spec, cand, allow_pre),
            _ => true,
        };
        if !ok {
            return false;
        }
    }
    true
}

fn meets_floors(req: &Requirement, cand: &Version, policy: Policy, strict_greater: bool) -> bool {
    for spec in &req.specifiers {
        let ok = match spec.op {
            Op::GtEq => cmp_local(cand, &spec.version) != Ordering::Less,
            Op::Gt => {
                if strict_greater {
                    satisfies_gt(&spec.version, cand)
                } else {
                    // Used only to detect "already current": `>` may not move
                    // if the newest release is the one `>` already names.
                    cmp_local(cand, &spec.version) != Ordering::Less
                }
            }
            Op::Compatible if policy.expand_compatible => {
                cmp_local(cand, &spec.version) != Ordering::Less
            }
            // When not expanding, `~=` is already enforced as a ceiling
            // (which includes its own lower bound).
            Op::Eq | Op::EqLegacy if policy.bump_pins => true,
            _ => true,
        };
        if !ok {
            return false;
        }
    }
    true
}

/// `>V` does not match a post-release or local build of `V` unless `V` is itself a post-release.
///
/// PEP 440 exclusive ordered comparison. Raw `>` would treat `1.0.post1` as newer than `1.0`
/// and the rewrite to `>=` would start allowing a release the old bound excluded.
fn satisfies_gt(spec: &Version, cand: &Version) -> bool {
    if same_release(spec, cand) && (cand.is_local() || (!spec.is_post() && cand.is_post())) {
        return false;
    }
    cmp_local(cand, spec) == Ordering::Greater
}

/// `<V` does not match a pre-release or dev release of `V` unless `V` is itself one.
///
/// `<2.0.0a5` still matches earlier pre-releases of `2.0.0` (`2.0.0a1`).
fn satisfies_lt(spec: &Version, cand: &Version) -> bool {
    if same_release(spec, cand) && !spec.any_prerelease() && cand.any_prerelease() {
        return false;
    }
    cmp_local(cand, spec) == Ordering::Less
}

fn same_release(left: &Version, right: &Version) -> bool {
    if left.epoch() != right.epoch() {
        return false;
    }
    let a = left.release();
    let b = right.release();
    let width = a.len().max(b.len());
    (0..width).all(|index| a.get(index).copied().unwrap_or(0) == b.get(index).copied().unwrap_or(0))
}

fn cmp_local(cand: &Version, spec: &Version) -> Ordering {
    cand.clone().without_local().cmp(spec)
}

fn not_equal(spec: &Specifier, cand: &Version) -> bool {
    if spec.star {
        // `!=1.0.*` excludes `1` and `1.0.0` (missing components pad with
        // zeros) but not `1.1`. Pre-releases of a matching prefix are excluded
        // too; `VersionSpecifier` drops them unless the specifier names one,
        // and its prefix check treats a shorter candidate as a match whenever
        // the shared prefix is equal (`==1.0.1.*` would match `1.0`).
        !prefix_match(&spec.version, cand)
    } else {
        cmp_local(cand, &spec.version) != Ordering::Equal
    }
}

fn compatible(spec: &Specifier, cand: &Version, allow_pre: bool) -> bool {
    if allow_pre {
        return compatible_series(spec, cand);
    }
    specifier_contains(spec, cand)
}

/// `~=X.Y` is `>=X.Y, ==X.*`. `~=X.Y.Z` is `>=X.Y.Z, ==X.Y.*`.
/// Pre-releases compare with normal PEP 440 ordering, so `--pre` can select
/// them inside the series.
fn compatible_series(spec: &Specifier, cand: &Version) -> bool {
    if cmp_local(cand, &spec.version) == Ordering::Less {
        return false;
    }
    let release = spec.version.release();
    if release.len() < 2 || cand.epoch() != spec.version.epoch() {
        return false;
    }
    release_has_prefix(cand.release(), &release[..release.len() - 1])
}

/// `prefix` matches `release` with missing components treated as zeros.
///
/// `==1.0.*` matches `1` and `1.0.2`. `==1.0.1.*` does not match `1.0`
/// (`1.0.0`). `==2.2.*` does not match `2`.
fn release_has_prefix(release: &[u64], prefix: &[u64]) -> bool {
    prefix
        .iter()
        .enumerate()
        .all(|(index, component)| release.get(index).copied().unwrap_or(0) == *component)
}

fn prefix_match(spec: &Version, cand: &Version) -> bool {
    spec.epoch() == cand.epoch() && release_has_prefix(cand.release(), spec.release())
}

/// A truncated `~=` spelling such as `~=2.33` does not admit `2.33.0rc1`.
/// Skip those candidates so `--pre` cannot narrow `~=2.28` (`==2.*`) into
/// `~=2.33.0rc1` (`==2.33.*`) just to name the pre-release.
fn compatible_spelling_admits(req: &Requirement, rel: &Release, _policy: Policy) -> bool {
    // `--expand-compatible` may leave the series, but the written `~=` still
    // has the original component count and must admit the chosen release.
    // `~=2.28` cannot become `~=3.0` to mean `3.0.0rc1`.
    for spec in &req.specifiers {
        if spec.op != Op::Compatible {
            continue;
        }
        let formatted = format_compatible(rel, spec.version.release().len());
        if !admits_compatible(&formatted, &rel.version) {
            return false;
        }
    }
    true
}

fn admits_compatible(formatted: &str, cand: &Version) -> bool {
    let Ok(written) = formatted.parse::<Version>() else {
        return false;
    };
    if cmp_local(cand, &written) == Ordering::Less {
        return false;
    }
    let release = written.release();
    if release.len() < 2 || cand.epoch() != written.epoch() {
        return false;
    }
    release_has_prefix(cand.release(), &release[..release.len() - 1])
}

fn specifier_contains(spec: &Specifier, cand: &Version) -> bool {
    let text = format!("{}{}", spec.op.pep440(), spec.version_text);
    VersionSpecifier::from_str(&text)
        .map(|s| s.contains(cand))
        .unwrap_or(false)
}

fn rewrite_specifier(spec: &Specifier, target: &Release, policy: Policy) -> Option<String> {
    if !is_rewritten_floor(spec, policy) {
        return None;
    }
    let replacement = match spec.op {
        Op::GtEq | Op::Eq | Op::EqLegacy => {
            if cmp_local(&target.version, &spec.version) == Ordering::Equal && !spec.star {
                return None;
            }
            format!("{}{}{}", spec.op.write(), spec.ws, target.version_text)
        }
        Op::Gt => {
            if !satisfies_gt(&spec.version, &target.version) {
                return None;
            }
            format!(">={}{}", spec.ws, target.version_text)
        }
        Op::Compatible => {
            let formatted = format_compatible(target, spec.version.release().len());
            if compatible_same(&spec.version, &formatted) {
                return None;
            }
            format!("~={}{}", spec.ws, formatted)
        }
        _ => return None,
    };
    let current = format!("{}{}{}", spec.op.write(), spec.ws, spec.version_text);
    if replacement == current {
        None
    } else {
        Some(replacement)
    }
}

fn compatible_same(old: &Version, formatted: &str) -> bool {
    formatted
        .parse::<Version>()
        .map(|v| v == *old)
        .unwrap_or(false)
}

/// Keep the original `~=` component count so `~=1.4` stays `==1.*` and
/// `~=1.4.2` stays `==1.4.*`.
fn format_compatible(target: &Release, components: usize) -> String {
    let n = components.max(1);
    let v = &target.version;
    if v.epoch() == 0
        && v.pre().is_none()
        && v.post().is_none()
        && v.dev().is_none()
        && !v.is_local()
        && v.release().len() == n
    {
        return target.version_text.clone();
    }
    if v.release().len() == n && !v.is_local() {
        return v.clone().without_local().to_string();
    }
    let mut parts: Vec<u64> = v.release().iter().copied().take(n).collect();
    while parts.len() < n {
        parts.push(0);
    }
    let body = parts
        .iter()
        .map(|p| p.to_string())
        .collect::<Vec<_>>()
        .join(".");
    if v.epoch() > 0 {
        format!("{}!{body}", v.epoch())
    } else {
        body
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::parse_requirement;

    fn rels(versions: &[&str]) -> Vec<Release> {
        versions
            .iter()
            .map(|v| Release::parse(v).unwrap())
            .collect()
    }

    fn policy(pins: bool, expand: bool, pre: bool) -> Policy {
        Policy {
            allow_pre: pre,
            bump_pins: pins,
            expand_compatible: expand,
        }
    }

    fn bump_text(src: &str, versions: &[&str], pol: Policy) -> String {
        let req = parse_requirement(src).unwrap();
        match decide(&req, Some(Lookup::Found(rels(versions))), pol) {
            Decision::Bump(edits) => {
                let mut out = src.to_string();
                let mut edits = edits;
                edits.sort_by_key(|edit| std::cmp::Reverse(edit.span.start));
                for edit in edits {
                    out.replace_range(edit.span, &edit.replacement);
                }
                out
            }
            Decision::Current => src.to_string(),
            Decision::Unbounded => src.to_string(),
            Decision::Note(reason, _) => format!("NOTE:{}", reason.as_str()),
        }
    }

    #[test]
    fn lower_bound_and_equality_of_trailing_zeros() {
        let pol = policy(false, false, false);
        assert_eq!(
            bump_text("requests>=2.28", &["2.28.0", "2.32.3", "2.33.0rc1"], pol),
            "requests>=2.32.3"
        );
        assert_eq!(
            bump_text("requests>=2.32", &["2.32.0"], pol),
            "requests>=2.32"
        );
        assert_eq!(
            bump_text("requests>=2.32.3", &["2.32.3"], pol),
            "requests>=2.32.3"
        );
    }

    #[test]
    fn pins_caps_and_compatible() {
        let pol = policy(false, false, false);
        assert_eq!(bump_text("httpx==0.27", &["0.28.1"], pol), "NOTE:exact_pin");
        assert_eq!(
            bump_text("httpcore==1.*", &["1.0.5", "2.0.0"], pol),
            "NOTE:exact_pin"
        );
        assert_eq!(
            bump_text("httpx==0.27", &["0.28.1"], policy(true, false, false)),
            "httpx==0.28.1"
        );
        assert_eq!(
            bump_text("django>=4.2,<5", &["4.2.16", "5.0.1"], pol),
            "django>=4.2.16,<5"
        );
        assert_eq!(
            bump_text("pkg>=4,<4", &["4.0.0"], pol),
            "NOTE:no_eligible_release"
        );
        assert_eq!(
            bump_text("requests~=2.28", &["2.28.1", "2.32.3", "3.0.0"], pol),
            "requests~=2.32"
        );
        assert_eq!(
            bump_text("requests~=2.28.1", &["2.28.5", "2.32.3"], pol),
            "requests~=2.28.5"
        );
        assert_eq!(
            bump_text("requests~=2.32", &["2.32.3"], pol),
            "requests~=2.32"
        );
        assert_eq!(
            bump_text("requests~=2.28", &["2.32.3"], policy(false, true, false)),
            "requests~=2.32"
        );
        assert_eq!(
            bump_text(
                "requests~=1.4.2",
                &["1.4.8", "2.3.1"],
                policy(false, true, false)
            ),
            "requests~=2.3.1"
        );
    }

    #[test]
    fn greater_than_becomes_inclusive_only_when_newer() {
        let pol = policy(false, false, false);
        assert_eq!(bump_text("pkg>1.0", &["1.2.0"], pol), "pkg>=1.2.0");
        assert_eq!(bump_text("pkg>  1", &["1.2.3"], pol), "pkg>=  1.2.3");
        assert_eq!(bump_text("pkg>1.0", &["1.0.0"], pol), "pkg>1.0");
        assert_eq!(bump_text("pkg>1.2.0", &["1.2.0"], pol), "pkg>1.2.0");
    }

    #[test]
    fn extras_markers_spacing_and_exclusions() {
        let pol = policy(false, false, false);
        assert_eq!(
            bump_text(
                "httpx[http2,brotli]>=0.27; python_version >= \"3.8\"",
                &["0.28.1"],
                pol
            ),
            "httpx[http2,brotli]>=0.28.1; python_version >= \"3.8\""
        );
        assert_eq!(
            bump_text("django (>=4.2, <5)", &["4.2.16", "5.1"], pol),
            "django (>=4.2.16, <5)"
        );
        assert_eq!(
            bump_text("flask  >=  2.0", &["3.0.3"], pol),
            "flask  >=  3.0.3"
        );
        assert_eq!(
            bump_text("idna>=2.0,!=2.8,!=2.9", &["2.8", "2.10", "3.10"], pol),
            "idna>=3.10,!=2.8,!=2.9"
        );
    }

    #[test]
    fn prereleases_yanked_and_local() {
        let stable = policy(false, false, false);
        assert_eq!(
            bump_text("pkg>=1.0", &["1.9.0", "2.0.0rc1"], stable),
            "pkg>=1.9.0"
        );
        assert_eq!(
            bump_text(
                "pkg>=1.0",
                &["1.9.0", "2.0.0rc1"],
                policy(false, false, true)
            ),
            "pkg>=2.0.0rc1"
        );
        assert_eq!(
            bump_text("pkg>=0.65b0", &["0.66.0", "0.70.0b1"], stable),
            "pkg>=0.70.0b1"
        );
        let mut yanked = rels(&["0.9.0", "1.0.0", "2.0.0"]);
        yanked[2].yanked = true;
        let req = parse_requirement("pkg>=0.1").unwrap();
        match decide(&req, Some(Lookup::Found(yanked)), stable) {
            Decision::Bump(edits) => assert_eq!(edits[0].replacement, ">=1.0.0"),
            other => panic!("unexpected {other:?}"),
        }
        let local = rels(&["0.9.0", "1.0.0", "2.0.0+local"]);
        let req = parse_requirement("pkg>=0.1").unwrap();
        match decide(&req, Some(Lookup::Found(local)), stable) {
            Decision::Bump(edits) => assert_eq!(edits[0].replacement, ">=1.0.0"),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn skips_without_a_floor() {
        let pol = policy(false, false, false);
        assert_eq!(bump_text("requests", &["2.0.0"], pol), "requests");
        assert_eq!(
            bump_text("pkg @ git+https://example.com/a.git", &["1.0.0"], pol),
            "NOTE:direct_reference"
        );
        assert_eq!(
            bump_text("pkg===1.0", &["2.0.0"], pol),
            "NOTE:arbitrary_equality"
        );
        assert_eq!(bump_text("pkg<2", &["1.9.0"], pol), "pkg<2");
    }

    #[test]
    fn pre_respects_compatible_series_and_star_exclusion() {
        let pre = policy(false, false, true);
        assert_eq!(
            bump_text("pkg~=2.28", &["2.32.3", "2.33.0rc1", "3.0.0rc1"], pre),
            "pkg~=2.32"
        );
        assert_eq!(
            bump_text("pkg~=2.28.0", &["2.28.4", "2.28.5rc1", "2.29.0"], pre),
            "pkg~=2.28.5rc1"
        );
        assert_eq!(
            bump_text("pkg~=2.28.1", &["2.28.5", "2.32.0rc1"], pre),
            "pkg~=2.28.5"
        );
        assert_eq!(
            bump_text("pkg>=1,!=2.*", &["1.5.0", "2.0.0rc1", "3.0.0"], pre),
            "pkg>=3.0.0,!=2.*"
        );
        // A pre-release named in the specifier opts into pre-releases even
        // without the global flag.
        let stable = policy(false, false, false);
        assert_eq!(
            bump_text("pkg~=1.0.0rc1", &["1.0.0rc2", "1.0.0", "1.1.0"], stable),
            "pkg~=1.0.0"
        );
    }

    #[test]
    fn greater_than_does_not_adopt_a_post_release_of_the_same_version() {
        let pol = policy(false, false, false);
        assert_eq!(
            bump_text("pkg>1.0", &["1.0.0", "1.0.0.post1"], pol),
            "pkg>1.0"
        );
        assert_eq!(bump_text("pkg>1.0.0", &["1.0.post1"], pol), "pkg>1.0.0");
        assert_eq!(
            bump_text("pkg>1", &["1.0.0.post1", "1.0.1"], pol),
            "pkg>=1.0.1"
        );
        assert_eq!(
            bump_text("pkg>1.0.post1", &["1.0.0.post2"], pol),
            "pkg>=1.0.0.post2"
        );
    }

    #[test]
    fn less_than_excludes_prereleases_of_the_cap() {
        let pre = policy(false, false, true);
        assert_eq!(
            bump_text("pkg>=1,<2", &["1.5.0", "2.0.0a1", "2.0.0.dev1"], pre),
            "pkg>=1.5.0,<2"
        );
        assert_eq!(
            bump_text(
                "pkg>=1.0.0a1,<2.0.0a5",
                &["1.0.0a1", "2.0.0a4", "2.0.0a5"],
                pre
            ),
            "pkg>=2.0.0a4,<2.0.0a5"
        );
        // A pre-release anywhere in the specifier opts in, same as --pre.
        let stable = policy(false, false, false);
        assert_eq!(
            bump_text("pkg>=0.65b0,<1", &["0.66.0", "0.70.0b1", "1.0.0a1"], stable),
            "pkg>=0.70.0b1,<1"
        );
    }

    #[test]
    fn star_exclusion_pads_missing_components() {
        let pol = policy(false, false, false);
        assert_eq!(
            bump_text("pkg>=0.5,<1.1,!=1.0.1.*", &["1.0", "1.0.1", "1.0.1.1"], pol),
            "pkg>=1.0,<1.1,!=1.0.1.*"
        );
        assert_eq!(
            bump_text("pkg>=0.1,<1.1,!=1.0.0.*", &["1.0", "1.0.0"], pol),
            "NOTE:no_eligible_release"
        );
        let pre = policy(false, false, true);
        assert_eq!(
            bump_text("pkg>=0.1,<1.1,!=1.0.*", &["1", "1.0.5", "1.0.0rc1"], pre),
            "NOTE:no_eligible_release"
        );
        assert_eq!(
            bump_text(
                "pkg>=1,<2,!=1.5",
                &["1.4.0", "1.5.0", "1.6.0", "2.0.0"],
                pol
            ),
            "pkg>=1.6.0,<2,!=1.5"
        );
    }

    #[test]
    fn wildcard_equality_is_not_an_exact_pin() {
        let pins = policy(true, false, false);
        assert_eq!(
            bump_text("pkg==1.*", &["1.2.0", "2.0.0"], pins),
            "NOTE:exact_pin"
        );
        assert_eq!(
            bump_text("pkg=1.*", &["1.2.0", "2.0.0"], pins),
            "NOTE:exact_pin"
        );
        assert_eq!(
            bump_text("pkg==1.2.3", &["1.2.3", "2.0.0"], pins),
            "pkg==2.0.0"
        );
        assert_eq!(bump_text("pkg=1.2.3", &["9.0.0"], pins), "pkg=9.0.0");
        assert_eq!(
            bump_text("pkg===1.2.3", &["9.0.0"], pins),
            "NOTE:arbitrary_equality"
        );
        let pol = policy(false, false, false);
        assert_eq!(
            bump_text("pkg>=1.0,==1.*", &["1.2.0", "1.9.5", "2.0.0"], pol),
            "pkg>=1.9.5,==1.*"
        );
        assert_eq!(
            bump_text("pkg==1.2.*,>=1", &["1.2.5", "1.3.0", "2.0.0"], pol),
            "pkg==1.2.*,>=1.2.5"
        );
    }

    #[test]
    fn compatible_spelling_admits_short_and_prerelease_targets() {
        let pol = policy(false, false, false);
        assert_eq!(bump_text("pkg~=1.0.0", &["1"], pol), "pkg~=1.0.0");
        assert_eq!(
            bump_text("pkg~=0.1.0", &["1"], pol),
            "NOTE:no_eligible_release"
        );
        let expand_pre = policy(false, true, true);
        assert_eq!(
            bump_text("pkg~=2.28", &["2.32.3", "3.0.0rc1"], expand_pre),
            "pkg~=2.32"
        );
        assert_eq!(
            bump_text("pkg~=2.28", &["3.0.0rc1"], expand_pre),
            "NOTE:no_eligible_release"
        );
        assert_eq!(
            bump_text(
                "pkg~=1.4.2",
                &["1.4.8", "2.3.1"],
                policy(false, true, false)
            ),
            "pkg~=2.3.1"
        );
        assert_eq!(
            bump_text("pkg~=2.28", &["2.28.1", "2.32.3"], pol),
            "pkg~=2.32"
        );
        assert_eq!(
            bump_text("pkg~=2.28.1", &["2.28.5", "2.32.3"], pol),
            "pkg~=2.28.5"
        );
    }

    #[test]
    fn epochs_locals_and_whitespace() {
        let pol = policy(false, false, false);
        assert_eq!(
            bump_text("pkg>=1!1.0", &["2.0.0", "1!1.4.0", "1!2.0.0"], pol),
            "pkg>=1!2.0.0"
        );
        assert_eq!(
            bump_text("pkg~=1!1.2", &["1!1.9.1", "1!2.0.0", "2.0.0"], pol),
            "pkg~=1!1.9"
        );
        assert_eq!(
            bump_text("pkg>=1.0,<1!1", &["2.0.0", "1!0.5.0"], pol),
            "pkg>=1!0.5.0,<1!1"
        );
        let mut local = rels(&["0.9.0", "2.0.0+cpu"]);
        local[1].version_text = "2.0.0+cpu".to_string();
        let req = parse_requirement("pkg>=1.0").unwrap();
        match decide(&req, Some(Lookup::Found(local)), pol) {
            Decision::Note(reason, _) => assert_eq!(reason, Reason::NoEligibleRelease),
            other => panic!("local release must not be selected: {other:?}"),
        }
        assert_eq!(
            bump_text(
                "pkg  >=  1.0 , < 2 , != 1.5",
                &["1.4.0", "1.5.0", "1.6.0"],
                pol
            ),
            "pkg  >=  1.6.0 , < 2 , != 1.5"
        );
        assert_eq!(
            bump_text("pkg~=1", &["1.2.0", "2.0.0"], pol),
            "NOTE:unparseable"
        );
    }
}
