use std::sync::Arc;

use regexrs::{
    BytesMode, Captures, CompileError, Error, Regex, RegexBuilder, RegexInput, RegexSet,
    RegexSetOptions, RegularRegex,
};

type Spans = Vec<Option<(usize, usize)>>;

/// Patterns inside the regular subset, including word boundaries, which
/// `Regex` runs on its backtracking VM.
const REGULAR: &[&str] = &[
    "",
    "a",
    "a*",
    "a*?",
    "(a|ab)(c|bcd)(d*)",
    r"(?<year>\d{4})-(?<month>\d\d)?",
    r"\b\w+\b",
    r"\B",
    r"\b",
    r"\<\w",
    r"\w\>",
    r"(?i)é+",
    r"(?i)k",
    "(?m)^a$",
    r"\Aa|b\z",
    "(?s).",
    ".",
    "[^a]",
    r"\d+|\p{L}+",
    "(?x) a b # comment",
    "a{2,3}?",
    "(a)|(b)",
    "(?:a|)+",
    "(a?)(b?)",
    "(?R)(?m)^$",
];

fn texts() -> Vec<String> {
    let alphabet = ["a", "b", "é", "K", " ", "\n", "1"];
    let mut all = vec![String::new()];
    let mut previous = vec![String::new()];
    for _ in 0..3 {
        let mut next = Vec::new();
        for prefix in &previous {
            for piece in alphabet {
                next.push(format!("{prefix}{piece}"));
            }
        }
        all.extend(next.iter().cloned());
        previous = next;
    }
    all.push("abcd ab1 1234-56 a\r\nb".into());
    all
}

fn spans_of(captures: &Captures<'_>) -> Spans {
    captures
        .iter()
        .map(|m| m.map(|m| (m.start(), m.end())))
        .collect()
}

/// Every search agrees with `Regex` for the same pattern: booleans, spans,
/// iteration, captures and replacement.
#[test]
fn regular_searches_equal_regex() {
    let texts = texts();
    for pattern in REGULAR {
        let regular = RegularRegex::new(pattern).unwrap();
        let regex = Regex::new(pattern).unwrap();
        assert_eq!(regular.captures_len(), regex.captures_len(), "{pattern}");
        assert!(
            regular.capture_names().eq(regex.capture_names()),
            "{pattern}"
        );
        for text in &texts {
            let text = text.as_str();
            let context = format!("{pattern} {text:?}");
            assert_eq!(
                regular.is_match(text),
                regex.is_match(text).unwrap(),
                "{context}"
            );
            assert_eq!(
                regular.find(text).map(|m| m.range()),
                regex.find(text).unwrap().map(|m| m.range()),
                "{context}"
            );
            let all: Vec<_> = regular.find_iter(text).map(|m| m.range()).collect();
            let expected: Vec<_> = regex.find_iter(text).map(|m| m.unwrap().range()).collect();
            assert_eq!(all, expected, "{context}");
            assert_eq!(
                regular.captures(text).as_ref().map(spans_of),
                regex.captures(text).unwrap().as_ref().map(spans_of),
                "{context}"
            );
            let captured: Vec<_> = regular.captures_iter(text).map(|c| spans_of(&c)).collect();
            let expected: Vec<_> = regex
                .captures_iter(text)
                .map(|c| spans_of(&c.unwrap()))
                .collect();
            assert_eq!(captured, expected, "{context}");
            assert_eq!(
                regular.replace_all(text, "<$0>"),
                regex.replace_all(text, "<$0>"),
                "{context}"
            );
            assert_eq!(
                regular.replacen(text, 2, |c: &Captures| c[0].len().to_string()),
                regex.replacen(text, 2, |c: &Captures| c[0].len().to_string()),
                "{context}"
            );
        }
    }
}

/// Ranges, interior UTF-8 starts and anchoring reach the regular engine with
/// the same meaning as for `Regex`.
#[test]
fn regular_inputs_respect_ranges_starts_and_anchors() {
    let text = "aé b\nabé";
    for pattern in [r"\b\w", "a|é", r"(\w)(\s)?", "(?m)^a", "", r"b\z"] {
        let regular = RegularRegex::new(pattern).unwrap();
        let regex = Regex::new(pattern).unwrap();
        for start in 0..=text.len() {
            for end in start..=text.len() {
                for anchored in [false, true] {
                    let input = RegexInput::new(text).range(start..end).anchored(anchored);
                    let context = format!("{pattern} {start}..{end} anchored={anchored}");
                    assert_eq!(
                        regular.is_match_input(input.clone()),
                        regex.is_match_input(input.clone()).unwrap(),
                        "{context}"
                    );
                    assert_eq!(
                        regular.find_input(input.clone()).map(|m| m.range()),
                        regex.find_input(input.clone()).unwrap().map(|m| m.range()),
                        "{context}"
                    );
                    assert_eq!(
                        regular.captures_input(input.clone()).as_ref().map(spans_of),
                        regex
                            .captures_input(input.clone())
                            .unwrap()
                            .as_ref()
                            .map(spans_of),
                        "{context}"
                    );
                    let all: Vec<_> = regular
                        .find_iter_input(input.clone())
                        .map(|m| m.range())
                        .collect();
                    let expected: Vec<_> = regex
                        .find_iter_input(input)
                        .map(|m| m.unwrap().range())
                        .collect();
                    assert_eq!(all, expected, "{context}");
                }
            }
            let input = RegexInput::new(text).from_pos(start);
            assert_eq!(
                regular.find_input(input.clone()).map(|m| m.range()),
                regex.find_input(input).unwrap().map(|m| m.range()),
                "{pattern} from {start}"
            );
        }
    }
}

/// Byte haystacks, including invalid UTF-8, in every bytes mode. Outside
/// ASCII mode, empty word-boundary matches next to invalid UTF-8 are covered
/// by `word_boundaries_next_to_invalid_utf8_test_every_position`.
#[test]
fn regular_byte_modes_equal_regex() {
    let haystacks: [&[u8]; 5] = [b"\xff\xffab", b"a\xc3\xa9 b", b"\xc3", b"", b"ab\xffab"];
    for mode in [
        BytesMode::Unicode,
        BytesMode::UnicodeBytes,
        BytesMode::Ascii,
    ] {
        for pattern in [r"\w+", r"(.)(.)?", r"\bb", "a|b", r"\B", r"\b"] {
            let mut builder = RegexBuilder::new(pattern);
            builder.bytes_mode(mode);
            let (Ok(regex), Ok(regular)) = (builder.build(), builder.build_regular()) else {
                panic!("{pattern} {mode:?} compiles in both");
            };
            for haystack in haystacks {
                let empty_boundary = matches!(pattern, r"\b" | r"\B");
                if empty_boundary
                    && mode != BytesMode::Ascii
                    && std::str::from_utf8(haystack).is_err()
                {
                    continue;
                }
                let context = format!("{pattern} {mode:?} {haystack:?}");
                assert_eq!(
                    regular.is_match(haystack),
                    regex.is_match(haystack).unwrap(),
                    "{context}"
                );
                let all: Vec<_> = regular.find_iter(haystack).map(|m| m.range()).collect();
                let expected: Vec<_> = regex
                    .find_iter(haystack)
                    .map(|m| m.unwrap().range())
                    .collect();
                assert_eq!(all, expected, "{context}");
                let captured: Vec<Spans> = regular
                    .captures_iter(haystack)
                    .map(|c| c.iter().map(|m| m.map(|m| (m.start(), m.end()))).collect())
                    .collect();
                let expected: Vec<Spans> = regex
                    .captures_iter(haystack)
                    .map(|c| {
                        c.unwrap()
                            .iter()
                            .map(|m| m.map(|m| (m.start(), m.end())))
                            .collect()
                    })
                    .collect();
                assert_eq!(captured, expected, "{context}");
            }
        }
    }
}

/// `Regex`'s backtracking engine advances over an invalid byte by the length
/// its leading bits imply, so it does not try every position next to invalid
/// UTF-8. `RegularRegex` tests every position, giving the `regex` crate's
/// byte-API answer.
#[test]
fn word_boundaries_next_to_invalid_utf8_test_every_position() {
    let haystacks: [&[u8]; 3] = [b"\xff\xffab", b"ab\xffab", b"\xc3 a\xff"];
    for pattern in [r"\b", r"\B", r"\b\w"] {
        let reference = regex::bytes::Regex::new(pattern).unwrap();
        let regular = RegexBuilder::new(pattern)
            .bytes_mode(BytesMode::UnicodeBytes)
            .build_regular()
            .unwrap();
        for haystack in haystacks {
            let expected: Vec<_> = reference.find_iter(haystack).map(|m| m.range()).collect();
            let found: Vec<_> = regular.find_iter(haystack).map(|m| m.range()).collect();
            assert_eq!(found, expected, "{pattern} {haystack:?}");
        }
    }
}

fn rejected(result: Result<RegularRegex, Error>) -> &'static str {
    match result {
        Err(Error::CompileError(error)) => match *error {
            CompileError::NotRegular(construct) => construct,
            other => panic!("unexpected compile error {other:?}"),
        },
        other => panic!("expected a NotRegular error, got {other:?}"),
    }
}

/// Rich constructs are rejected by name, including ones `Regex` currently
/// optimizes onto its regular engine.
#[test]
fn rich_constructs_are_rejected_by_name() {
    for (pattern, construct) in [
        (r"(a)\1", "backreference"),
        (r"(?<n>a)\k<n>", "backreference"),
        ("a(?=b)", "lookaround"),
        ("(?<=a)b", "lookaround"),
        ("x(?!y)", "lookaround"),
        ("(?>a)", "atomic group or possessive quantifier"),
        ("a++", "atomic group or possessive quantifier"),
        ("(a)?(?(1)b|c)", "conditional"),
        (r"a\Kb", r"`\K`"),
        (r"\Ga", r"`\G`"),
        (r"\R", r"`\R` (general newline)"),
        (r"a\Z", r"`\Z` (end before trailing newlines)"),
        (r"(?<n>a)\g<n>", "subroutine call"),
        ("(?~abc)", "absent operator"),
        ("x|(?(DEFINE)(?<a>y))", "DEFINE group"),
    ] {
        assert_eq!(rejected(RegularRegex::new(pattern)), construct, "{pattern}");
        assert!(Regex::new(pattern).is_ok(), "Regex accepts {pattern}");
    }
    let onig = RegexBuilder::new("(?m)^a")
        .oniguruma_mode(true)
        .build_regular();
    assert_eq!(rejected(onig), "Oniguruma's multi-line `^`");
}

/// Options only the backtracking engine implements are rejected; options
/// that only tune it are accepted and change nothing.
#[test]
fn rich_only_options_are_explicit() {
    for (configure, option) in [
        (
            (|b: &mut RegexBuilder| {
                b.find_not_empty(true);
            }) as fn(&mut RegexBuilder),
            "the find_not_empty option",
        ),
        (
            |b: &mut RegexBuilder| {
                b.disallow_empty_match_at_eof_after_newline(true);
            },
            "the disallow_empty_match_at_eof_after_newline option",
        ),
        (
            |b: &mut RegexBuilder| {
                b.allow_input_assertion_overrides(true);
            },
            "the allow_input_assertion_overrides option",
        ),
    ] {
        let mut builder = RegexBuilder::new("a+");
        configure(&mut builder);
        assert_eq!(rejected(builder.build_regular()), option);
    }
    let tuned = RegexBuilder::new(r"\ba+")
        .backtrack_limit(1)
        .seek(true)
        .native(false)
        .build_regular()
        .unwrap();
    assert_eq!(tuned.find(" aaaa").map(|m| m.range()), Some(1..5));
}

/// A converted expression shares the compiled engine and still matches
/// exactly, including through a searcher and a set.
#[test]
fn conversion_into_regex_preserves_matching() {
    let regular = RegularRegex::new(r"\b(?<word>\w+)\b").unwrap();
    let regex = Regex::from(regular.clone());
    let text = "é ab, Kc";
    let expected: Vec<_> = regular.captures_iter(text).map(|c| spans_of(&c)).collect();
    let converted: Vec<_> = regex
        .captures_iter(text)
        .map(|c| spans_of(&c.unwrap()))
        .collect();
    assert_eq!(converted, expected);
    let mut search = regex.searcher();
    let mut locations = regex.capture_locations();
    search.captures_read(&mut locations, text).unwrap();
    assert_eq!(locations.name("word"), Some((0, 2)));
    // The set's candidate search drops the word boundaries, so it also finds
    // "ab" inside "cab"; verification must reject that and find the real match.
    let bounded = Regex::from(RegularRegex::new(r"\bab").unwrap());
    let set = RegexSet::from_regexes(
        [Arc::new(bounded), Arc::new(Regex::new("zz").unwrap())],
        RegexSetOptions::default(),
    )
    .unwrap();
    let mut found = set.find_input(RegexInput::new("cab ab")).unwrap().unwrap();
    let first = found.next().unwrap().unwrap();
    assert_eq!((first.pattern(), first.start(), first.end()), (0, 4, 6));
}

#[test]
fn regular_regex_parses_and_displays() {
    let regular: RegularRegex = r"\d+".parse().unwrap();
    assert_eq!(regular.to_string(), r"\d+");
    assert_eq!(format!("{regular:?}"), r"\d+");
    assert_eq!(regular.as_str(), r"\d+");
    assert!(r"(\d)\1".parse::<RegularRegex>().is_err());
}
