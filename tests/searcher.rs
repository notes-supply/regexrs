use regexrs::{BytesMode, CaptureLocations, Regex, RegexBuilder, RegexInput};

type Spans = Vec<Option<(usize, usize)>>;

fn match_spans<'t, S: regexrs::Input + ?Sized + 't>(
    matches: impl Iterator<Item = regexrs::Result<S::Match<'t>>>,
    span: impl Fn(&S::Match<'t>) -> (usize, usize),
) -> Vec<Result<(usize, usize), String>> {
    matches
        .map(|m| m.map(|m| span(&m)).map_err(|e| e.to_string()))
        .collect()
}

fn capture_spans<'t>(
    captures: impl Iterator<Item = regexrs::Result<regexrs::Captures<'t>>>,
) -> Vec<Result<Spans, String>> {
    captures
        .map(|c| {
            c.map(|c| c.iter().map(|m| m.map(|m| (m.start(), m.end()))).collect())
                .map_err(|e| e.to_string())
        })
        .collect()
}

fn location_spans(locations: &CaptureLocations) -> Spans {
    (0..locations.len()).map(|i| locations.get(i)).collect()
}

const PATTERNS: &[&str] = &[
    r"",
    r"a*",
    r"(a|ab)\1",
    r"(?i:(é)\1)",
    r"(\w+)\s+\1",
    r"(?<=ab)c",
    r"a+(?=c)",
    r"key-(?!731\b)\d+",
    r"(?P<x>ab)(?P=x)",
    r"(?>a|ab)c",
    r"a++a",
    r"(a)?(?(1)b|c)",
    r"((a|ab)+)\1",
    r"\b\w+\b",
    r"(?m:^a+$)",
    r"(?s:(.+)\1)",
    r"(?:(?<=(a))b)\1",
    r"(?:a|(?=b))*b",
];

fn strings() -> Vec<String> {
    let alphabet = ["a", "b", "c", "é", "É", " ", "\n"];
    let mut all = vec![String::new()];
    let mut previous = vec![String::new()];
    for _ in 0..4 {
        let mut next = Vec::new();
        for prefix in previous {
            for suffix in alphabet {
                next.push(format!("{prefix}{suffix}"));
            }
        }
        all.extend(next.iter().cloned());
        previous = next;
    }
    all
}

#[test]
fn reusable_searcher_matches_published_oracle_for_boolean_spans_and_captures() {
    let texts = strings();
    for pattern in PATTERNS {
        let oracle = oracle::Regex::new(pattern).unwrap();
        let regex = Regex::new(pattern).unwrap();
        let mut search = regex.searcher();
        for text in &texts {
            let text = text.as_str();
            assert_eq!(
                search.is_match(text).map_err(|e| e.to_string()),
                oracle.is_match(text).map_err(|e| e.to_string()),
                "boolean {pattern} {text:?}"
            );
            assert_eq!(
                search
                    .find(text)
                    .map(|m| m.map(|m| (m.start(), m.end())))
                    .map_err(|e| e.to_string()),
                oracle
                    .find(text)
                    .map(|m| m.map(|m| (m.start(), m.end())))
                    .map_err(|e| e.to_string()),
                "find {pattern} {text:?}"
            );
            assert_eq!(
                search
                    .captures(text)
                    .map(|c| c.map(|c| c
                        .iter()
                        .map(|m| m.map(|m| (m.start(), m.end())))
                        .collect::<Vec<_>>()))
                    .map_err(|e| e.to_string()),
                oracle
                    .captures(text)
                    .map(|c| c.map(|c| c
                        .iter()
                        .map(|m| m.map(|m| (m.start(), m.end())))
                        .collect::<Vec<_>>()))
                    .map_err(|e| e.to_string()),
                "captures {pattern} {text:?}"
            );
        }
    }
    eprintln!("oracle comparisons: {}", PATTERNS.len() * texts.len() * 3);
}

#[test]
fn searcher_preserves_ranged_anchored_and_assertion_context() {
    for pattern in [r"a+(?=b)", r"(?<=a)b", r"(a)\1", r"\Aa", r"\b", r"$"] {
        let regex = Regex::new(pattern).unwrap();
        let mut search = regex.searcher();
        for text in ["aab", "ab", "éaab", " aa", "", "a\n"] {
            for start in 0..=text.len() + 1 {
                for anchored in [false, true] {
                    let input = RegexInput::new(text).from_pos(start).anchored(anchored);
                    assert_eq!(
                        search
                            .find_input(input.clone())
                            .map(|m| m.map(|m| (m.start(), m.end())))
                            .map_err(|e| e.to_string()),
                        regex
                            .find_input(input.clone())
                            .map(|m| m.map(|m| (m.start(), m.end())))
                            .map_err(|e| e.to_string())
                    );
                    assert_eq!(
                        search
                            .is_match_input(input.clone())
                            .map_err(|e| e.to_string()),
                        regex.is_match_input(input).map_err(|e| e.to_string())
                    );
                }
            }
        }
    }
}

#[test]
fn captures_are_owned_snapshots_not_reused_answers() {
    for pattern in [r"(\w+)", r"(\w+)\s+\1"] {
        let regex = Regex::new(pattern).unwrap();
        let mut search = regex.searcher();
        let first = search.captures("one one").unwrap().unwrap();
        let _ = search.captures("two two").unwrap();
        let _ = search.captures("").unwrap();
        assert_eq!(first.get(1).unwrap().as_str(), "one");
    }
}

#[test]
fn byte_searcher_preserves_invalid_utf8_and_ascii_matching() {
    for pattern in [r"(.)\1", r"(.+)", r"a+(?=b)", r"(?<=a)b"] {
        let regex = RegexBuilder::new(pattern)
            .bytes_mode(BytesMode::Ascii)
            .build()
            .unwrap();
        let mut search = regex.searcher();
        for text in [
            &b"\xff\xffab"[..],
            &b"aab"[..],
            &b"\xc3\xa9\xc3\xa9"[..],
            &b""[..],
        ] {
            assert_eq!(
                search.is_match(text).map_err(|e| e.to_string()),
                regex.is_match(text).map_err(|e| e.to_string())
            );
            assert_eq!(
                search
                    .find(text)
                    .map(|m| m.map(|m| (m.start(), m.end())))
                    .map_err(|e| e.to_string()),
                regex
                    .find(text)
                    .map(|m| m.map(|m| (m.start(), m.end())))
                    .map_err(|e| e.to_string())
            );
        }
    }
}

#[test]
fn limit_errors_do_not_poison_later_searches() {
    let pattern = r"(a+)+\1b";
    let regex = RegexBuilder::new(pattern)
        .backtrack_limit(20)
        .build()
        .unwrap();
    let oracle = oracle::RegexBuilder::new(pattern)
        .backtrack_limit(20)
        .build()
        .unwrap();
    let mut search = regex.searcher();
    for text in ["aaaaaaaaaaaaaaaaaaac", "aab", "", "aaaaaab"] {
        assert_eq!(
            search.is_match(text).map_err(|e| e.to_string()),
            oracle.is_match(text).map_err(|e| e.to_string())
        );
    }
    assert!(!regex.has_native_code());
}

#[cfg(all(feature = "native", target_os = "linux", target_arch = "x86_64"))]
#[test]
fn native_candidate_and_unsupported_fallback_keep_full_functionality() {
    let accelerated = Regex::new(r"(a|ab)\1").unwrap();
    assert!(accelerated.has_native_code());
    assert!(accelerated.is_match("abab").unwrap());
    let portable = RegexBuilder::new(r"(a|ab)\1")
        .native(false)
        .build()
        .unwrap();
    assert!(!portable.has_native_code());
    let unsupported = Regex::new(r"((a|ab)+)\1").unwrap();
    assert!(!unsupported.has_native_code());
    assert!(unsupported.is_match("abab").unwrap());
    let large = "a".repeat(100_002);
    let budget = Regex::new(r"\A(a+)\1\z").unwrap();
    let oracle = oracle::Regex::new(r"\A(a+)\1\z").unwrap();
    assert!(budget.has_native_code());
    assert_eq!(
        budget.is_match(&large).map_err(|e| e.to_string()),
        oracle.is_match(&large).map_err(|e| e.to_string())
    );
}

/// One searcher reused across every pattern's texts iterates exactly like the
/// pooled `Regex` iterators, including empty matches at UTF-8 boundaries.
#[test]
fn searcher_iteration_equals_regex_iteration() {
    let texts = strings();
    for pattern in PATTERNS {
        let regex = Regex::new(pattern).unwrap();
        let mut search = regex.searcher();
        for text in &texts {
            let text = text.as_str();
            let span = |m: &regexrs::Match<'_>| (m.start(), m.end());
            assert_eq!(
                match_spans::<str>(search.find_iter(text), span),
                match_spans::<str>(regex.find_iter(text), span),
                "find_iter {pattern} {text:?}"
            );
            assert_eq!(
                capture_spans(search.captures_iter(text)),
                capture_spans(regex.captures_iter(text)),
                "captures_iter {pattern} {text:?}"
            );
        }
    }
}

#[test]
fn searcher_iteration_covers_ranges_and_bytes() {
    let regex = RegexBuilder::new(r"(.)\1|a*")
        .bytes_mode(BytesMode::Ascii)
        .build()
        .unwrap();
    let mut search = regex.searcher();
    let text = &b"\xff\xffaab\xc3\xa9"[..];
    let span = |m: &regexrs::MatchBytes<'_>| (m.start(), m.end());
    for start in 0..=text.len() {
        for end in start..=text.len() {
            let input = RegexInput::new(text).range(start..end);
            assert_eq!(
                match_spans::<[u8]>(search.find_iter_input(input.clone()), span),
                match_spans::<[u8]>(regex.find_iter_input(input), span),
                "{start}..{end}"
            );
        }
    }
}

/// Iteration ends after the first error, like `Regex::find_iter`, and the
/// searcher's scratch is clean for the next search.
#[test]
fn searcher_iteration_stops_after_an_error_and_recovers() {
    let regex = RegexBuilder::new(r"x|(a+)+\1$")
        .backtrack_limit(100)
        .build()
        .unwrap();
    let text = "x x aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa!";
    let mut search = regex.searcher();
    let span = |m: &regexrs::Match<'_>| (m.start(), m.end());
    let found = match_spans::<str>(search.find_iter(text), span);
    assert_eq!(found, match_spans::<str>(regex.find_iter(text), span));
    assert_eq!(found.len(), 3);
    assert_eq!(found[..2], [Ok((0, 1)), Ok((2, 3))]);
    assert!(found[2].is_err());
    let captured = capture_spans(search.captures_iter(text));
    assert_eq!(captured, capture_spans(regex.captures_iter(text)));
    assert!(captured.last().unwrap().is_err());
    assert_eq!(search.find("aa").unwrap().map(|m| m.range()), Some(0..2));
}

/// Dropping an iterator part-way leaves no state behind in the scratch.
#[test]
fn partially_consumed_searcher_iterators_do_not_leak_state() {
    for pattern in [r"(\w)\1", r"\w+", r"(?<=a)b|c"] {
        let regex = Regex::new(pattern).unwrap();
        let mut search = regex.searcher();
        let first = search
            .find_iter("aa bb abc cc")
            .next()
            .map(|m| m.unwrap().range());
        assert_eq!(
            first,
            regex.find("aa bb abc cc").unwrap().map(|m| m.range())
        );
        let _ = search.captures_iter("abab bbcc").nth(1);
        for text in ["zz", "", "ab c", "cc"] {
            assert_eq!(
                search.find(text).unwrap().map(|m| m.range()),
                regex.find(text).unwrap().map(|m| m.range()),
                "{pattern} {text:?}"
            );
        }
    }
}

/// Caller-owned locations equal owned captures on hits and read as unmatched
/// after misses, for the regular, rich and rewritten group-0 routes.
#[test]
fn capture_locations_follow_hits_and_misses() {
    for pattern in [
        r"(\d+)-(\d+)?",
        r"(\w+)\s+\1",
        r"(\w)(?=!)",
        r"(?<word>a)|(b)",
    ] {
        let regex = Regex::new(pattern).unwrap();
        let mut search = regex.searcher();
        let mut locations = regex.capture_locations();
        assert_eq!(locations.len(), regex.captures_len());
        assert_eq!(location_spans(&locations), vec![None; regex.captures_len()]);
        for text in [
            "12-34",
            "nothing here",
            "7-",
            "",
            "go go",
            "!",
            "x!",
            "b",
            "a",
        ] {
            let expected = search.captures(text).unwrap();
            let found = search.captures_read(&mut locations, text).unwrap();
            assert_eq!(
                found.map(|m| m.range()),
                expected.as_ref().map(|c| c.get(0).unwrap().range()),
                "{pattern} {text:?}"
            );
            let expected_spans = match &expected {
                Some(c) => c.iter().map(|m| m.map(|m| (m.start(), m.end()))).collect(),
                None => vec![None; regex.captures_len()],
            };
            assert_eq!(
                location_spans(&locations),
                expected_spans,
                "{pattern} {text:?}"
            );
        }
    }
    for pattern in [r"(\d)", r"(\d)\1"] {
        let regex = Regex::new(pattern).unwrap();
        let mut search = regex.searcher();
        let mut locations = regex.capture_locations();
        assert!(
            search
                .captures_read(&mut locations, "77")
                .unwrap()
                .is_some()
        );
        let past_end = RegexInput::new("77").from_pos(3);
        assert!(
            search
                .captures_read_input(&mut locations, past_end)
                .unwrap()
                .is_none()
        );
        assert_eq!(location_spans(&locations), vec![None; regex.captures_len()]);
    }
    let named = Regex::new(r"(?<word>\w+)\s+\k<word>").unwrap();
    let mut search = named.searcher();
    let mut locations = named.capture_locations();
    search
        .captures_read(&mut locations, "say hello hello")
        .unwrap();
    assert_eq!(locations.name("word"), Some((4, 9)));
    assert_eq!(locations.name("missing"), None);
}

/// An error clears the locations; later searches reuse them normally.
#[test]
fn capture_locations_are_cleared_by_errors() {
    let regex = RegexBuilder::new(r"(a+)+\1$")
        .backtrack_limit(100)
        .build()
        .unwrap();
    let mut search = regex.searcher();
    let mut locations = regex.capture_locations();
    assert!(
        search
            .captures_read(&mut locations, "aa")
            .unwrap()
            .is_some()
    );
    assert_eq!(locations.get(1), Some((0, 1)));
    assert!(
        search
            .captures_read(&mut locations, "aaaaaaaaaaaaaaaaaaaaaaaa!")
            .is_err()
    );
    assert_eq!(location_spans(&locations), vec![None, None]);
    assert!(
        search
            .captures_read(&mut locations, "aaaa")
            .unwrap()
            .is_some()
    );
    assert_eq!(locations.get(0), Some((0, 4)));
}

/// Locations from another regex are replaced with this regex's storage.
#[test]
fn capture_locations_from_another_regex_are_replaced() {
    let rich = Regex::new(r"(\w)(\w)\2").unwrap();
    let regular = Regex::new(r"(\d)").unwrap();
    let mut locations = rich.capture_locations();
    let mut search = regular.searcher();
    assert!(
        search
            .captures_read(&mut locations, "a7")
            .unwrap()
            .is_some()
    );
    assert_eq!(locations.len(), 2);
    assert_eq!(location_spans(&locations), vec![Some((1, 2)), Some((1, 2))]);
    let mut search = rich.searcher();
    assert!(
        search
            .captures_read(&mut locations, "xyy")
            .unwrap()
            .is_some()
    );
    assert_eq!(locations.len(), 3);
    assert_eq!(locations.get(2), Some((1, 2)));
}

#[test]
fn capture_locations_respect_ranges_and_anchors() {
    for pattern in [r"(a)(b)?", r"(a)\1", r"(?<=a)(b)"] {
        let regex = Regex::new(pattern).unwrap();
        let mut search = regex.searcher();
        let mut locations = regex.capture_locations();
        let text = "aabab";
        for start in 0..=text.len() {
            for end in start..=text.len() {
                for anchored in [false, true] {
                    let input = RegexInput::new(text).range(start..end).anchored(anchored);
                    let expected = regex.captures_input(input.clone()).unwrap();
                    search.captures_read_input(&mut locations, input).unwrap();
                    let expected_spans = match &expected {
                        Some(c) => c.iter().map(|m| m.map(|m| (m.start(), m.end()))).collect(),
                        None => vec![None; regex.captures_len()],
                    };
                    assert_eq!(
                        location_spans(&locations),
                        expected_spans,
                        "{pattern} {start}..{end}"
                    );
                }
            }
        }
    }
}
