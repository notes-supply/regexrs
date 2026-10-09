use regexrs::{BytesMode, Regex, RegexBuilder, RegexInput};

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
