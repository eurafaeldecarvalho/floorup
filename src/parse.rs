//! Span-preserving PEP 508 requirement parser.
//!
//! Only the version-specifier slices are interesting to the editor. Names,
//! extras, markers, and surrounding whitespace stay where they were so a later
//! splice can replace specifier text without reformatting the line.

use pep440_rs::Version;
use std::ops::Range;

/// Comparison operator, in source order (longest match wins).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Arbitrary,
    Compatible,
    LtEq,
    GtEq,
    Eq,
    NotEq,
    Lt,
    Gt,
    EqLegacy,
}

impl Op {
    /// Operator spelling accepted by `pep440_rs`.
    pub fn pep440(self) -> &'static str {
        match self {
            Op::Arbitrary => "===",
            Op::Compatible => "~=",
            Op::LtEq => "<=",
            Op::GtEq => ">=",
            Op::Eq | Op::EqLegacy => "==",
            Op::NotEq => "!=",
            Op::Lt => "<",
            Op::Gt => ">",
        }
    }

    /// Operator spelling to write back. Legacy `=` stays `=`.
    pub fn write(self) -> &'static str {
        match self {
            Op::EqLegacy => "=",
            other => other.pep440(),
        }
    }

    pub fn is_pin(self) -> bool {
        matches!(self, Op::Eq | Op::EqLegacy)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Specifier {
    pub op: Op,
    /// Byte range of the operator inside the (trimmed) requirement.
    pub op_span: Range<usize>,
    /// Whitespace between the operator and the version, preserved on rewrite.
    pub ws: String,
    pub version_span: Range<usize>,
    /// Version token as written, including a trailing `.*` and a leading `v`.
    pub version_text: String,
    pub version: Version,
    pub star: bool,
}

impl Specifier {
    /// Byte range covering the operator, the separating whitespace, and the version.
    pub fn span(&self) -> Range<usize> {
        self.op_span.start..self.version_span.end
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Requirement {
    pub name: String,
    pub name_span: Range<usize>,
    pub is_url: bool,
    pub specifiers: Vec<Specifier>,
    /// Byte offset applied to spans so they refer to the original decoded string,
    /// not the trimmed core that was parsed.
    trim_start: usize,
}

impl Requirement {
    pub fn span_in_decoded(&self, span: Range<usize>) -> Range<usize> {
        span.start + self.trim_start..span.end + self.trim_start
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub message: String,
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

pub fn parse_requirement(input: &str) -> Result<Requirement, ParseError> {
    let trim_start = leading_ws(input);
    let trim_end = trailing_ws_index(input);
    if trim_start > trim_end {
        return Err(err("empty requirement"));
    }
    let core = &input[trim_start..trim_end];
    let mut req = parse_core(core)?;
    req.trim_start = trim_start;
    Ok(req)
}

/// True when `s` is exactly one version specifier (`>=1.2`, `==1.*`, `>  1`).
pub fn is_single_specifier(s: &str) -> bool {
    let mut cur = Cur::new(s);
    match parse_specifier(&mut cur) {
        Ok(_) => cur.eof() && !s.is_empty(),
        Err(_) => false,
    }
}

fn parse_core(input: &str) -> Result<Requirement, ParseError> {
    let mut cur = Cur::new(input);
    cur.skip_ws();
    let name_span = parse_name(&mut cur)?;
    let name = input[name_span.clone()].to_string();
    cur.skip_ws();
    if cur.starts_with("[") {
        parse_extras(&mut cur)?;
        cur.skip_ws();
    }
    if cur.starts_with("@") {
        return Ok(Requirement {
            name,
            name_span,
            is_url: true,
            specifiers: Vec::new(),
            trim_start: 0,
        });
    }

    let paren = if cur.starts_with("(") {
        cur.bump(1);
        cur.skip_ws();
        true
    } else {
        false
    };

    let mut specifiers = Vec::new();
    loop {
        cur.skip_ws();
        if cur.eof() || cur.starts_with(")") || cur.starts_with(";") {
            break;
        }
        specifiers.push(parse_specifier(&mut cur)?);
        cur.skip_ws();
        if cur.starts_with(",") {
            cur.bump(1);
            continue;
        }
        break;
    }

    if paren {
        cur.skip_ws();
        if !cur.starts_with(")") {
            return Err(err("unclosed '(' in version specifier"));
        }
        cur.bump(1);
    }

    cur.skip_ws();
    if cur.starts_with(";") || cur.eof() {
        Ok(Requirement {
            name,
            name_span,
            is_url: false,
            specifiers,
            trim_start: 0,
        })
    } else {
        Err(err(format!(
            "unexpected trailing input {rest:?}",
            rest = cur.rest()
        )))
    }
}

fn parse_specifier(cur: &mut Cur<'_>) -> Result<Specifier, ParseError> {
    let op_start = cur.i;
    let op = parse_op(cur)?;
    let op_end = cur.i;
    cur.skip_ws();
    let version_start = cur.i;
    let ws = cur.s[op_end..version_start].to_string();
    let version_text = if op == Op::Arbitrary {
        parse_arbitrary(cur)?
    } else {
        parse_version_token(cur)?
    };
    let version_end = cur.i;
    if version_text.is_empty() {
        return Err(err("missing version"));
    }
    let (version, star) = if op == Op::Arbitrary {
        // Arbitrary equality is a string match. A dummy version keeps the
        // struct uniform; callers never order on it.
        (Version::new([0]), false)
    } else {
        parse_pep440(&version_text).map_err(|message| ParseError { message })?
    };
    if star && !matches!(op, Op::Eq | Op::EqLegacy | Op::NotEq) {
        return Err(err("a trailing .* is only valid with == or !=".to_string()));
    }
    Ok(Specifier {
        op,
        op_span: op_start..op_end,
        ws,
        version_span: version_start..version_end,
        version_text,
        version,
        star,
    })
}

fn parse_pep440(token: &str) -> Result<(Version, bool), String> {
    let star = token.ends_with(".*");
    let core = if star {
        &token[..token.len() - 2]
    } else {
        token
    };
    if core.is_empty() {
        return Err(format!("invalid version {token:?}"));
    }
    if let Ok(version) = core.parse::<Version>() {
        return Ok((version, star));
    }
    let stripped = core
        .strip_prefix('v')
        .or_else(|| core.strip_prefix('V'))
        .unwrap_or(core);
    match stripped.parse::<Version>() {
        Ok(version) => Ok((version, star)),
        Err(e) => Err(format!("invalid version {token:?}: {e}")),
    }
}

fn parse_op(cur: &mut Cur<'_>) -> Result<Op, ParseError> {
    const OPS: &[(&str, Op)] = &[
        ("===", Op::Arbitrary),
        ("~=", Op::Compatible),
        ("<=", Op::LtEq),
        (">=", Op::GtEq),
        ("==", Op::Eq),
        ("!=", Op::NotEq),
        ("<", Op::Lt),
        (">", Op::Gt),
        ("=", Op::EqLegacy),
    ];
    for (text, op) in OPS {
        if cur.starts_with(text) {
            cur.bump(text.len());
            return Ok(*op);
        }
    }
    Err(err(format!(
        "expected a version operator, found {rest:?}",
        rest = cur.rest()
    )))
}

fn parse_arbitrary(cur: &mut Cur<'_>) -> Result<String, ParseError> {
    let start = cur.i;
    while let Some(b) = cur.peek() {
        if b.is_ascii_whitespace() || matches!(b, b',' | b')' | b';') {
            break;
        }
        cur.bump(1);
    }
    if cur.i == start {
        return Err(err("missing arbitrary version"));
    }
    Ok(cur.s[start..cur.i].to_string())
}

fn parse_version_token(cur: &mut Cur<'_>) -> Result<String, ParseError> {
    let start = cur.i;
    while let Some(b) = cur.peek() {
        if b.is_ascii_alphanumeric() || matches!(b, b'.' | b'*' | b'!' | b'+' | b'-' | b'_') {
            cur.bump(1);
        } else {
            break;
        }
    }
    if cur.i == start {
        return Err(err("missing version"));
    }
    Ok(cur.s[start..cur.i].to_string())
}

fn parse_name(cur: &mut Cur<'_>) -> Result<Range<usize>, ParseError> {
    let start = cur.i;
    let bytes = cur.s.as_bytes();
    if start >= bytes.len() || !is_name_start(bytes[start]) {
        return Err(err("missing package name"));
    }
    let mut end = start + 1;
    while end < bytes.len() && is_name_byte(bytes[end]) {
        end += 1;
    }
    while end > start && matches!(bytes[end - 1], b'.' | b'-' | b'_') {
        end -= 1;
    }
    if end == start {
        return Err(err("missing package name"));
    }
    cur.i = end;
    Ok(start..end)
}

fn parse_extras(cur: &mut Cur<'_>) -> Result<(), ParseError> {
    if !cur.starts_with("[") {
        return Err(err("expected '['"));
    }
    cur.bump(1);
    cur.skip_ws();
    if cur.starts_with("]") {
        cur.bump(1);
        return Ok(());
    }
    loop {
        cur.skip_ws();
        parse_name(cur)?;
        cur.skip_ws();
        if cur.starts_with(",") {
            cur.bump(1);
            continue;
        }
        break;
    }
    cur.skip_ws();
    if !cur.starts_with("]") {
        return Err(err("unclosed '[' in extras"));
    }
    cur.bump(1);
    Ok(())
}

fn is_name_start(b: u8) -> bool {
    b.is_ascii_alphanumeric()
}

fn is_name_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.')
}

fn leading_ws(s: &str) -> usize {
    s.char_indices()
        .find(|(_, c)| !c.is_whitespace())
        .map(|(i, _)| i)
        .unwrap_or(s.len())
}

fn trailing_ws_index(s: &str) -> usize {
    s.trim_end().len()
}

fn err(message: impl Into<String>) -> ParseError {
    ParseError {
        message: message.into(),
    }
}

struct Cur<'a> {
    s: &'a str,
    i: usize,
}

impl<'a> Cur<'a> {
    fn new(s: &'a str) -> Self {
        Self { s, i: 0 }
    }

    fn rest(&self) -> &'a str {
        &self.s[self.i..]
    }

    fn eof(&self) -> bool {
        self.i >= self.s.len()
    }

    fn peek(&self) -> Option<u8> {
        self.s.as_bytes().get(self.i).copied()
    }

    fn starts_with(&self, prefix: &str) -> bool {
        self.rest().starts_with(prefix)
    }

    fn bump(&mut self, n: usize) {
        self.i += n;
    }

    fn skip_ws(&mut self) {
        while let Some(b) = self.peek() {
            if b == b' ' || b == b'\t' {
                self.i += 1;
            } else {
                break;
            }
        }
    }
}

/// PEP 503 normalization: lowercase, and collapse runs of `-`, `_`, and `.` to `-`.
pub fn normalize_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut dash = false;
    for c in name.chars() {
        if c == '-' || c == '_' || c == '.' {
            if !dash {
                out.push('-');
                dash = true;
            }
        } else {
            dash = false;
            for lower in c.to_lowercase() {
                out.push(lower);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec_slice(req_src: &str, index: usize) -> &str {
        let req = parse_requirement(req_src).unwrap();
        let spec = &req.specifiers[index];
        let span = req.span_in_decoded(spec.span());
        &req_src[span]
    }

    #[test]
    fn parses_lower_bound_marker_and_extra() {
        let src = "Requests[security, socks] >= 2.28 ; python_version >= '3.8'";
        let req = parse_requirement(src).unwrap();
        assert_eq!(req.name, "Requests");
        assert!(!req.is_url);
        assert_eq!(req.specifiers.len(), 1);
        assert_eq!(req.specifiers[0].op, Op::GtEq);
        assert_eq!(req.specifiers[0].version_text, "2.28");
        assert_eq!(spec_slice(src, 0), ">= 2.28");
        assert!(!src[req.specifiers[0].version_span.end + req.trim_start..].contains("2.28"));
    }

    #[test]
    fn marker_comparison_is_not_a_specifier() {
        let src = "brotli; platform_python_implementation == 'CPython'";
        let req = parse_requirement(src).unwrap();
        assert!(req.specifiers.is_empty());
        assert!(!req.is_url);
    }

    #[test]
    fn parentheses_spacing_and_star() {
        let src = "django (>=4.2, < 5.0)";
        let req = parse_requirement(src).unwrap();
        assert_eq!(req.specifiers.len(), 2);
        assert_eq!(spec_slice(src, 0), ">=4.2");
        assert_eq!(spec_slice(src, 1), "< 5.0");

        let star = parse_requirement("httpcore==1.*").unwrap();
        assert!(star.specifiers[0].star);
        assert_eq!(star.specifiers[0].version_text, "1.*");
        assert_eq!(star.specifiers[0].op, Op::Eq);
    }

    #[test]
    fn direct_url_and_arbitrary() {
        let url = parse_requirement("pkg[extra] @ git+https://example.com/pkg.git").unwrap();
        assert!(url.is_url);
        assert!(url.specifiers.is_empty());

        let arb = parse_requirement("pkg===abc.def").unwrap();
        assert_eq!(arb.specifiers[0].op, Op::Arbitrary);
        assert_eq!(arb.specifiers[0].version_text, "abc.def");
    }

    #[test]
    fn legacy_equal_and_compatible_and_prerelease() {
        let legacy = parse_requirement("pkg=1.2.3").unwrap();
        assert_eq!(legacy.specifiers[0].op, Op::EqLegacy);
        let compat = parse_requirement("packaging~=23.1").unwrap();
        assert_eq!(compat.specifiers[0].op, Op::Compatible);
        assert_eq!(compat.specifiers[0].version.release(), &[23, 1]);
        let pre = parse_requirement("opentelemetry-instrumentation-fastapi>=0.65b0").unwrap();
        assert!(pre.specifiers[0].version.any_prerelease());
        assert_eq!(pre.specifiers[0].version_text, "0.65b0");
    }

    #[test]
    fn trims_multiline_padding() {
        let src = "\n  requests>=2.28\n  ";
        let req = parse_requirement(src).unwrap();
        let span = req.span_in_decoded(req.specifiers[0].span());
        assert_eq!(&src[span], ">=2.28");
    }

    #[test]
    fn rejects_junk_and_does_not_panic_on_short_input() {
        assert!(parse_requirement("").is_err());
        assert!(parse_requirement("...").is_err());
        assert!(parse_requirement("pkg >=").is_err());
        assert!(parse_requirement("pkg >=1.0 trailing").is_err());
        assert!(parse_requirement("pkg~=1").is_ok());
    }

    #[test]
    fn single_specifier_recognizer() {
        assert!(is_single_specifier(">=1.2.3"));
        assert!(is_single_specifier(">  1.0"));
        assert!(is_single_specifier("==1.*"));
        assert!(is_single_specifier("~=1.4.2"));
        assert!(!is_single_specifier(">=1,<2"));
        assert!(!is_single_specifier("requests>=1"));
        assert!(!is_single_specifier("# comment"));
    }

    #[test]
    fn normalize_pep503() {
        assert_eq!(normalize_name("Requests"), "requests");
        assert_eq!(normalize_name("Foo_Bar.Baz"), "foo-bar-baz");
        assert_eq!(normalize_name("A__B"), "a-b");
        assert_eq!(normalize_name("typing-extensions"), "typing-extensions");
    }

    proptest::proptest! {
        #![proptest_config(proptest::test_runner::Config::with_cases(200))]
        #[test]
        fn parse_never_panics(s in "[ -~]{0,80}") {
            let _ = parse_requirement(&s);
        }
    }

    #[test]
    fn parser_fuzz_smoke() {
        let samples = [
            "",
            " ",
            "a",
            "a[",
            "a[]",
            "a[b,c]",
            "a>=1",
            "a >= 1.2.3",
            "a==1.*",
            "a===",
            "a @ http://x",
            "a(;)",
            "a(>=1,)",
            "a>=1; x >= \"1\"",
            "h2>=3,<5",
            "name_with.dot-and_more>=1!2.0.post1.dev2",
        ];
        for sample in samples {
            let _ = parse_requirement(sample);
        }
    }
}
