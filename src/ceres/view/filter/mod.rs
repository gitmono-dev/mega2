mod canonical;
mod parse;

pub use canonical::{canonicalize, print};
use sha2::{Digest, Sha256};

pub const REGISTER_MAX_SPEC_BYTES: usize = 16_384;
pub const REGISTER_MAX_NESTING: usize = 16;
pub const PARSE_MAX_NESTING: usize = 64;

const _: () = assert!(REGISTER_MAX_NESTING < PARSE_MAX_NESTING);
const _: () = assert!(PARSE_MAX_NESTING >= 16);

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ViewPath {
    segments: Vec<String>,
}

impl ViewPath {
    pub fn new(segments: Vec<String>) -> Self {
        assert!(
            !segments.is_empty() && segments.iter().all(|segment| valid_path_segment(segment)),
            "ViewPath requires valid, non-empty path segments"
        );
        Self { segments }
    }

    pub fn segments(&self) -> &[String] {
        &self.segments
    }
}

fn valid_path_segment(segment: &str) -> bool {
    !segment.is_empty()
        && segment != "."
        && segment != ".."
        && !segment.contains(['/', '\\'])
        && !segment.chars().any(char::is_control)
        && !segment.chars().next().is_some_and(char::is_whitespace)
        && !segment.chars().last().is_some_and(char::is_whitespace)
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Selector {
    Entry(ViewPath),
    Tree(ViewPath),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Filter {
    Subdir(ViewPath),
    Prefix(ViewPath),
    Exclude(Vec<Selector>),
    Compose(Vec<Filter>),
    Chain(Vec<Filter>),
    Nop,
    Empty,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CanonicalFilter {
    pub filter: Filter,
    pub canonical_text: String,
    pub filter_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum FilterParseError {
    #[error("backslash is not allowed in a path")]
    Backslash,
    #[error("a path segment has edge whitespace")]
    SegmentEdgeWhitespace,
    #[error("invalid path segment")]
    InvalidSegment,
    #[error("path needs quoting")]
    NeedsQuoting,
    #[error("path may only be quoted as a whole")]
    PartialQuote,
    #[error("unterminated quoted path")]
    UnterminatedQuote,
    #[error("control character in path")]
    ControlChar,
    #[error("empty selector segment")]
    EmptySelectorSegment,
    #[error("exclude arguments must be selectors")]
    ExcludeArgNotSelector,
    #[error("invalid filter syntax")]
    Syntax,
    #[error("filter spec exceeds the registration byte limit")]
    SpecTooLarge,
    #[error("filter nesting exceeds the supported limit")]
    NestingTooDeep,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecheckFailure {
    RoundTrip,
    FilterIdMismatch,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("filter definition is corrupt: {failed:?}")]
pub struct DefinitionCorrupt {
    pub failed: RecheckFailure,
}

pub fn parse(text: &str) -> Result<Filter, FilterParseError> {
    parse::parse_with_max(text, PARSE_MAX_NESTING)
}

pub fn filter_id(canonical_text: &str) -> String {
    let mut hash = Sha256::new();
    hash.update(b"mega-view-filter/v1\n");
    hash.update(canonical_text.as_bytes());
    hex::encode(hash.finalize())
}

pub fn parse_for_registration(spec: &str) -> Result<CanonicalFilter, FilterParseError> {
    if spec.len() > REGISTER_MAX_SPEC_BYTES {
        return Err(FilterParseError::SpecTooLarge);
    }
    let filter = canonicalize(parse::parse_with_max(spec, REGISTER_MAX_NESTING)?);
    let canonical_text = print(&filter);
    let filter_id = filter_id(&canonical_text);
    Ok(CanonicalFilter {
        filter,
        canonical_text,
        filter_id,
    })
}

pub fn recheck_definition(
    canonical_spec: &str,
    expected_filter_id: &str,
) -> Result<CanonicalFilter, DefinitionCorrupt> {
    let filter = parse(canonical_spec)
        .map(canonicalize)
        .map_err(|_| DefinitionCorrupt {
            failed: RecheckFailure::RoundTrip,
        })?;
    let canonical_text = print(&filter);
    if canonical_text != canonical_spec {
        return Err(DefinitionCorrupt {
            failed: RecheckFailure::RoundTrip,
        });
    }
    let actual_id = filter_id(canonical_spec);
    if actual_id != expected_filter_id {
        return Err(DefinitionCorrupt {
            failed: RecheckFailure::FilterIdMismatch,
        });
    }
    Ok(CanonicalFilter {
        filter,
        canonical_text,
        filter_id: actual_id,
    })
}

#[cfg(test)]
mod tests {
    use rand::{RngExt, SeedableRng, rngs::StdRng};

    use super::*;

    const GOLDEN: [(&str, &str, &str); 11] = [
        (
            ":/project/foo/",
            ":/project/foo",
            "8fc638cef18f06b23a788f7eb21c085cb4b7aa40974f29010e2e7e60f045834f",
        ),
        (
            ":/\"project/foo\"",
            ":/project/foo",
            "8fc638cef18f06b23a788f7eb21c085cb4b7aa40974f29010e2e7e60f045834f",
        ),
        (
            ":prefix=\"a,b\"",
            ":prefix=\"a,b\"",
            "f238ead9656130686004bc34d7c61ddc8fe2796386c7df1a8302f33fd6bb6f6b",
        ),
        (
            ":/\"x]y\"",
            ":/\"x]y\"",
            "728494f84405f553e3e4406b2effd9414944dd0446eb44c038b9b36a6265ded5",
        ),
        (
            ":/\"say \\\"hi\\\"\"",
            ":/\"say \\\"hi\\\"\"",
            "a4fd2c31643a45a650264530a24c3e7e7e23051b532e91153cfd9edc022d80ed",
        ),
        (
            ":/a:exclude[ ::b , ::\"c,d\"/ ]",
            ":/a:exclude[::\"c,d\"/,::b]",
            "37f8ba0aef0540241ae5ebb80ea21c34c2487f511cfd047b9189dec7d00544a2",
        ),
        (
            ":/\"my dir\"",
            ":/\"my dir\"",
            "f60323fbf08cc5a9533db2ca50f337cd2ac5205006c43712055a21d8b5986dc4",
        ),
        (
            ":[:/b:prefix=y, :/a:prefix=x]",
            ":[:/a:prefix=x,:/b:prefix=y]",
            "4e77701b623b885b03e6e6a766f6eb7af6d163f169a5fa6d0fa5ecf71309f3d1",
        ),
        (
            ":[:/a:prefix=a,:/B:prefix=b]",
            ":[:/B:prefix=b,:/a:prefix=a]",
            "f9ed38b3cc202c93c54af197b4433547900cbc7be559a0df2ca103b8b05c8a8b",
        ),
        (
            ":/中文/目录",
            ":/中文/目录",
            "aa95db5b3ad2f600dd183a2f96a62b1743e27cb7ec2809b7526ae4e86a07ff6e",
        ),
        (
            ":/\"中文/目录\"",
            ":/中文/目录",
            "aa95db5b3ad2f600dd183a2f96a62b1743e27cb7ec2809b7526ae4e86a07ff6e",
        ),
    ];

    fn path(parts: &[&str]) -> ViewPath {
        ViewPath::new(parts.iter().map(|part| (*part).to_owned()).collect())
    }

    fn canonical(input: &str) -> String {
        print(&canonicalize(parse(input).unwrap()))
    }

    fn d(depth: usize) -> String {
        let mut text = ":[:/x,:/y]".to_owned();
        for _ in 1..depth {
            text = format!(":[:/x{text},:/y]");
        }
        text
    }

    #[test]
    fn golden_vectors_match() {
        for (input, expected, id) in &GOLDEN {
            let got = parse_for_registration(input).unwrap();
            assert_eq!(got.canonical_text, *expected, "{input}");
            assert_eq!(got.filter_id, *id, "{input}");
        }
    }

    #[test]
    fn rejects_invalid_inputs() {
        use FilterParseError::*;
        let cases = [
            (":/a\\b", Backslash),
            (":/\"a\\b\"", Backslash),
            (":/\"a \"", SegmentEdgeWhitespace),
            (":/\" a\"", SegmentEdgeWhitespace),
            (":/\"\u{3000}a\"", SegmentEdgeWhitespace),
            (":/\"a\u{3000}\"", SegmentEdgeWhitespace),
            (":/a/../b", InvalidSegment),
            (":/a/./b", InvalidSegment),
            (":/a//b", InvalidSegment),
            (":/", InvalidSegment),
            (":/\"\"", InvalidSegment),
            (":///a", InvalidSegment),
            (":/a//", InvalidSegment),
            (":prefix=//a", InvalidSegment),
            (":/\"//a\"", InvalidSegment),
            (":/my dir", NeedsQuoting),
            (":exclude[::my dir]", NeedsQuoting),
            (":/. x", NeedsQuoting),
            (":/a/../b c", NeedsQuoting),
            (":/a \u{1}", NeedsQuoting),
            (":/a=b", NeedsQuoting),
            (":/web/\"@types\"", PartialQuote),
            (":/\"a\"b", PartialQuote),
            (":/\"a\"\\b", Backslash),
            (":/\"a\"@", Syntax),
            (":/\"a\"\u{1}", Syntax),
            (":/\"a", UnterminatedQuote),
            (":/\"a\tb\"", ControlChar),
            (":/\"a\0b\"", ControlChar),
            (":/\"a\u{7f}b\"", ControlChar),
            (":/\"a\nb\"", ControlChar),
            (":/a\u{80}b", ControlChar),
            (":/a\u{1}b", ControlChar),
            (":exclude[::\"a b/\"]", EmptySelectorSegment),
            (":exclude[sub1=:/sub3]", ExcludeArgNotSelector),
            (":/a :prefix=b", Syntax),
            (":/. :/x", Syntax),
            (":/a/../b :/c", Syntax),
            (":nop x", Syntax),
            (":/\"a b\" c", Syntax),
            (":[:/a] x", Syntax),
            (":exclude[::a] x", Syntax),
            (":exclude [::a]", Syntax),
        ];
        for (input, expected) in cases {
            assert_eq!(parse_for_registration(input), Err(expected), "{input:?}");
        }
    }

    #[test]
    fn register_parse_limits() {
        assert!(parse_for_registration(&format!(":/{}", "a".repeat(16_382))).is_ok());
        assert_eq!(
            parse_for_registration(&format!(":/{}", "a".repeat(16_383))),
            Err(FilterParseError::SpecTooLarge)
        );
        assert!(parse_for_registration(&format!(":/{}ab", "中".repeat(5_460))).is_ok());
        assert_eq!(
            parse_for_registration(&format!(":/{}", "中".repeat(5_461))),
            Err(FilterParseError::SpecTooLarge)
        );
        assert!(parse_for_registration(&d(16)).is_ok());
        assert_eq!(
            parse_for_registration(&d(17)),
            Err(FilterParseError::NestingTooDeep)
        );
        assert_eq!(
            parse_for_registration(&":[".repeat(8_000)),
            Err(FilterParseError::NestingTooDeep)
        );
        assert_eq!(
            parse_for_registration(&":[".repeat(9_000)),
            Err(FilterParseError::SpecTooLarge)
        );
    }

    #[test]
    fn canonicalize_rules() {
        let cases = [
            (":/a/", ":/a"),
            (":prefix=/a/", ":prefix=a"),
            ("://a", ":/a"),
            (":/\"/a/\"", ":/a"),
            (":exclude[::a/]", ":exclude[::a/]"),
            (":/a:/b", ":/a/b"),
            (":prefix=a:prefix=b", ":prefix=b/a"),
            (":prefix=p:/p", ":nop"),
            (":prefix=q/r:/q", ":prefix=r"),
            (":prefix=p:/p/r", ":/r"),
            (":prefix=a:/b", ":empty"),
            (":prefix=ab:/a", ":empty"),
            (":nop:/a", ":/a"),
            (":nop:nop", ":nop"),
            (":/a:empty:prefix=b", ":empty"),
            (":[:empty,:/a]", ":/a"),
            (":[:empty]", ":empty"),
            (":[:nop,:/a]", ":[:/a,:nop]"),
            (":[:/b,:[ :/a,:/a]]", ":[:/a,:/a,:/b]"),
            (":exclude[::b,::a,::b]", ":exclude[::a,::b]"),
            (":prefix=a:prefix=b:/b", ":prefix=a"),
        ];
        for (input, expected) in cases {
            assert_eq!(canonical(input), expected, "{input}");
        }
        let long_subdirs = ":/a".repeat(5_461);
        assert_eq!(
            parse_for_registration(&long_subdirs)
                .unwrap()
                .canonical_text,
            format!(":/{}", vec!["a"; 5_461].join("/"))
        );
        let long_prefixes = ":prefix=a".repeat(1_638);
        assert_eq!(
            parse_for_registration(&long_prefixes)
                .unwrap()
                .canonical_text,
            format!(":prefix={}", vec!["a"; 1_638].join("/"))
        );
        assert_eq!(canonicalize(Filter::Exclude(vec![])), Filter::Nop);
        assert_eq!(
            canonicalize(Filter::Chain(vec![
                Filter::Subdir(path(&["a"])),
                Filter::Exclude(vec![])
            ])),
            Filter::Subdir(path(&["a"]))
        );
        assert_eq!(
            print(&canonicalize(Filter::Compose(vec![
                Filter::Exclude(vec![]),
                Filter::Subdir(path(&["a"]))
            ]))),
            ":[:/a,:nop]"
        );
        let nested = Filter::Chain(vec![
            Filter::Chain(vec![
                Filter::Subdir(path(&["a"])),
                Filter::Prefix(path(&["x"])),
            ]),
            Filter::Exclude(vec![Selector::Entry(path(&["b"]))]),
        ]);
        assert!(matches!(canonicalize(nested), Filter::Chain(parts) if parts.len() == 3));
    }

    const SEED: u64 = 0x2026_1002_0a02;
    const PARTS: [&str; 15] = [
        "a",
        "B",
        "0",
        "...",
        ".a",
        "中文",
        "目录",
        "a b",
        "a　b",
        "a,b",
        "x]y",
        "say \"hi\"",
        "\"x",
        "a=b",
        "_-",
    ];

    fn random_path(rng: &mut StdRng) -> ViewPath {
        let count = rng.random_range(1..=3);
        ViewPath::new(
            (0..count)
                .map(|_| PARTS[rng.random_range(0..PARTS.len())].to_owned())
                .collect(),
        )
    }

    fn random_filter(rng: &mut StdRng, depth: usize) -> Filter {
        let choice = rng.random_range(0..if depth == 0 { 5 } else { 7 });
        match choice {
            0 => Filter::Subdir(random_path(rng)),
            1 => Filter::Prefix(random_path(rng)),
            2 => Filter::Exclude(
                (0..rng.random_range(1..=4))
                    .map(|_| {
                        if rng.random_bool(0.5) {
                            Selector::Entry(random_path(rng))
                        } else {
                            Selector::Tree(random_path(rng))
                        }
                    })
                    .collect(),
            ),
            3 => Filter::Nop,
            4 => Filter::Empty,
            5 => Filter::Compose(
                (0..rng.random_range(2..=4))
                    .map(|_| random_filter(rng, depth - 1))
                    .collect(),
            ),
            _ => Filter::Chain(
                (0..rng.random_range(2..=4))
                    .map(|_| random_filter(rng, depth - 1))
                    .collect(),
            ),
        }
    }

    fn assert_canonical(filter: &Filter, context: &str) {
        assert_eq!(&canonicalize(filter.clone()), filter, "{context}");
        match filter {
            Filter::Subdir(p) | Filter::Prefix(p) => assert_path(p, context),
            Filter::Exclude(selectors) => {
                assert!(!selectors.is_empty(), "{context}");
                for selector in selectors {
                    match selector {
                        Selector::Entry(p) | Selector::Tree(p) => assert_path(p, context),
                    }
                }
                let texts: Vec<_> = selectors.iter().map(canonical::print_selector).collect();
                assert!(texts.windows(2).all(|w| w[0] < w[1]), "{context}");
            }
            Filter::Compose(members) => {
                assert!(members.len() >= 2, "{context}");
                assert!(
                    members
                        .iter()
                        .all(|m| !matches!(m, Filter::Compose(_) | Filter::Empty)),
                    "{context}"
                );
                assert!(
                    members.windows(2).all(|w| print(&w[0]) <= print(&w[1])),
                    "{context}"
                );
                for member in members {
                    assert_canonical(member, context);
                }
            }
            Filter::Chain(ops) => {
                assert!(ops.len() >= 2, "{context}");
                assert!(
                    ops.iter()
                        .all(|op| !matches!(op, Filter::Chain(_) | Filter::Nop | Filter::Empty)),
                    "{context}"
                );
                for op in ops {
                    assert_canonical(op, context);
                }
            }
            Filter::Nop | Filter::Empty => {}
        }
    }

    fn assert_path(path: &ViewPath, context: &str) {
        assert!(!path.segments.is_empty(), "{context}");
        for part in &path.segments {
            assert!(!part.is_empty() && part != "." && part != "..", "{context}");
            assert!(
                !part.contains(['/', '\\']) && !part.chars().any(char::is_control),
                "{context}"
            );
            assert!(!part.chars().next().unwrap().is_whitespace(), "{context}");
            assert!(!part.chars().last().unwrap().is_whitespace(), "{context}");
        }
    }

    #[test]
    fn roundtrip_print_parse() {
        let mut rng = StdRng::seed_from_u64(SEED);
        for sample in 0..10_000 {
            let ast = canonicalize(random_filter(&mut rng, 4));
            let context = format!("seed={SEED} sample={sample}");
            assert_canonical(&ast, &context);
            let text = print(&ast);
            assert_eq!(parse(&text), Ok(ast), "{context} text={text}");
        }
    }

    fn noncanonical(filter: &Filter, rng: &mut StdRng) -> String {
        match filter {
            Filter::Subdir(p) | Filter::Prefix(p) => {
                if p.segments.len() > 1 && rng.random_bool(0.5) {
                    let first = ViewPath::new(vec![p.segments[0].clone()]);
                    let rest = ViewPath::new(p.segments[1..].to_vec());
                    return if matches!(filter, Filter::Subdir(_)) {
                        format!(
                            "{}{}",
                            noncanonical(&Filter::Subdir(first), rng),
                            noncanonical(&Filter::Subdir(rest), rng)
                        )
                    } else {
                        format!(
                            "{}{}",
                            noncanonical(&Filter::Prefix(rest), rng),
                            noncanonical(&Filter::Prefix(first), rng)
                        )
                    };
                }
                let op = if matches!(filter, Filter::Subdir(_)) {
                    ":/"
                } else {
                    ":prefix="
                };
                let mut value = canonical::print_path(p);
                if rng.random_bool(0.5) && !value.starts_with('"') {
                    value = format!("\"{value}\"");
                }
                if rng.random_bool(0.5) {
                    value = if value.starts_with('"') {
                        format!("\"/{}\"", &value[1..value.len() - 1])
                    } else {
                        format!("/{value}")
                    };
                }
                if rng.random_bool(0.5) {
                    value = if value.starts_with('"') {
                        format!("\"{}/\"", &value[1..value.len() - 1])
                    } else {
                        format!("{value}/")
                    };
                }
                format!("{op}{value}")
            }
            Filter::Exclude(selectors) => {
                let mut items: Vec<_> = selectors.iter().map(canonical::print_selector).collect();
                shuffle(&mut items, rng);
                if !items.is_empty() && rng.random_bool(0.5) {
                    items.push(items[0].clone());
                }
                format!(
                    ":exclude[{}{}{}]",
                    whitespace(rng),
                    items.join(&format!("{} , {}", whitespace(rng), whitespace(rng))),
                    whitespace(rng)
                )
            }
            Filter::Compose(members) => {
                let mut items: Vec<_> = members.iter().map(|m| noncanonical(m, rng)).collect();
                shuffle(&mut items, rng);
                if items.len() > 1 && rng.random_bool(0.5) {
                    let pair = format!(":[{},{}]", items.remove(0), items.remove(0));
                    items.insert(0, pair);
                }
                format!(
                    ":[{}{}{}]",
                    whitespace(rng),
                    items.join(&format!("{} , {}", whitespace(rng), whitespace(rng))),
                    whitespace(rng)
                )
            }
            Filter::Chain(ops) => ops.iter().map(|op| noncanonical(op, rng)).collect(),
            Filter::Nop => ":nop".to_owned(),
            Filter::Empty => ":empty".to_owned(),
        }
    }

    fn shuffle<T>(items: &mut [T], rng: &mut StdRng) {
        for index in (1..items.len()).rev() {
            items.swap(index, rng.random_range(0..=index));
        }
    }

    fn whitespace(rng: &mut StdRng) -> &'static str {
        [" ", "\t", "\n", "\r"][rng.random_range(0..4)]
    }

    #[test]
    fn roundtrip_canonical_fixed_point() {
        let seed = SEED ^ 1;
        let mut rng = StdRng::seed_from_u64(seed);
        for sample in 0..10_000 {
            let ast = canonicalize(random_filter(&mut rng, 4));
            let context = format!("seed={seed} sample={sample}");
            assert_canonical(&ast, &context);
            let input = format!(
                "{}{}{}",
                whitespace(&mut rng),
                noncanonical(&ast, &mut rng),
                whitespace(&mut rng)
            );
            let first =
                parse(&input).unwrap_or_else(|e| panic!("{context} input={input:?} error={e:?}"));
            let y = print(&canonicalize(first));
            assert_eq!(y, print(&ast), "{context} input={input:?}");
            assert_eq!(
                print(&canonicalize(parse(&y).unwrap_or_else(|e| panic!(
                    "{context} output={y:?} error={e:?}"
                )))),
                y,
                "{context} input={input:?}"
            );
        }
    }

    #[test]
    fn load_recheck_accepts_valid() {
        for (_, canonical, id) in &GOLDEN {
            let got = recheck_definition(canonical, id).unwrap();
            assert_eq!(got.filter, parse(canonical).unwrap());
        }
        let mut large = vec![format!(":/{}", "a".repeat(16_383)), d(17), d(64)];
        large.push(format!(
            ":[{}]",
            (0..65)
                .map(|i| format!(":/m{i:02}:prefix=n{i:02}"))
                .collect::<Vec<_>>()
                .join(",")
        ));
        large.push(format!(
            ":exclude[{}]",
            (0..257)
                .map(|i| format!("::s{i:03}"))
                .collect::<Vec<_>>()
                .join(",")
        ));
        for spec in large {
            let got = recheck_definition(&spec, &filter_id(&spec)).unwrap();
            assert_eq!(got.filter, parse(&spec).unwrap(), "{spec}");
        }
    }

    #[test]
    fn load_recheck_rejects_corrupt() {
        let invalid = [
            ":/project/foo/".to_owned(),
            " :/project/foo".to_owned(),
            ":[:/b:prefix=y,:/a:prefix=x]".to_owned(),
            ":/\"a".to_owned(),
            d(65),
            ":[".repeat(8_000),
        ];
        for spec in invalid {
            assert_eq!(
                recheck_definition(&spec, &filter_id(&spec)),
                Err(DefinitionCorrupt {
                    failed: RecheckFailure::RoundTrip
                }),
                "{spec}"
            );
        }
        assert_eq!(
            recheck_definition(":/project/foo/", &"0".repeat(64)),
            Err(DefinitionCorrupt {
                failed: RecheckFailure::RoundTrip
            })
        );
        let id = GOLDEN[0].2;
        assert_eq!(
            recheck_definition(":/project/foo", &format!("0{}", &id[1..])),
            Err(DefinitionCorrupt {
                failed: RecheckFailure::FilterIdMismatch
            })
        );
        assert_eq!(
            recheck_definition(":/project/foo", &id.to_uppercase()),
            Err(DefinitionCorrupt {
                failed: RecheckFailure::FilterIdMismatch
            })
        );
    }
}
