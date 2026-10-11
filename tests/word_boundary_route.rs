//! Patterns that need the backtracking VM only for word boundaries run on
//! automata where that provably gives the VM's results, and on the VM
//! elsewhere. Every observable result must equal the published oracle.

use regexrs::{BytesMode, Error, Regex, RegexBuilder, RegexInput, RuntimeError};

type Spans = Vec<Option<(usize, usize)>>;

const PATTERNS: &[&str] = &[
    r"\b",
    r"\B",
    r"\b\w+\b",
    r"\bab\b",
    r"\Ba",
    r"a\B",
    r"\<\w",
    r"\w\>",
    r"(?i)\bk\w*",
    r"\b(\w)(\w)?\b",
    r"(?m)^\b\w|\b$",
    r"\b(?:fn|let|é)\b",
    r"(?s)\b.\b",
    r"\b\d+\b|\b[[:alpha:]]+",
    // Unbounded repetitions of expressions that can match empty keep the VM;
    // bounded ones agree with the automata.
    r"\b(a|)*",
    r"\b(a|)*b",
    r"(a|)*\bb",
    r"(?:\b|a)+b",
    r"(\b\w*)*",
    r"\b(a|){2,3}",
];

fn texts() -> Vec<String> {
    let alphabet = ["a", "b", "é", "K", "\u{212A}", " ", "\n", "1", "-"];
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
    all.push("fn let é_ab ab-ab 12 a\u{212A}b".into());
    all
}

fn result<T, E: std::fmt::Debug>(r: Result<T, E>) -> Result<T, String> {
    r.map_err(|e| {
        format!("{e:?}")
            .split('(')
            .next()
            .unwrap_or_default()
            .to_string()
    })
}

/// Every API on whole texts: booleans, spans, iteration, captures and
/// replacement equal the oracle.
#[test]
fn word_boundary_patterns_equal_the_oracle() {
    let texts = texts();
    for pattern in PATTERNS {
        let regex = Regex::new(pattern).unwrap();
        let oracle = oracle::Regex::new(pattern).unwrap();
        for text in &texts {
            let text = text.as_str();
            let context = format!("{pattern} {text:?}");
            assert_eq!(
                result(regex.is_match(text)),
                result(oracle.is_match(text)),
                "{context}"
            );
            let found: Result<Vec<_>, _> = regex
                .find_iter(text)
                .map(|m| m.map(|m| m.range()))
                .collect();
            let expected: Result<Vec<_>, _> = oracle
                .find_iter(text)
                .map(|m| m.map(|m| m.range()))
                .collect();
            assert_eq!(result(found), result(expected), "{context}");
            let captured: Result<Vec<Spans>, _> = regex
                .captures_iter(text)
                .map(|c| c.map(|c| c.iter().map(|m| m.map(|m| (m.start(), m.end()))).collect()))
                .collect();
            let expected: Result<Vec<Spans>, _> = oracle
                .captures_iter(text)
                .map(|c| c.map(|c| c.iter().map(|m| m.map(|m| (m.start(), m.end()))).collect()))
                .collect();
            assert_eq!(result(captured), result(expected), "{context}");
            assert_eq!(
                regex.replace_all(text, "<$0>"),
                oracle.replace_all(text, "<$0>"),
                "{context}"
            );
        }
    }
}

/// Ranges, starts inside characters and anchoring, which decide the route
/// per search, give the oracle's answers on either route.
#[test]
fn ranged_and_interior_searches_equal_the_oracle() {
    let text = "é ab\u{212A}c é-ab";
    for pattern in PATTERNS {
        let regex = Regex::new(pattern).unwrap();
        let oracle = oracle::Regex::new(pattern).unwrap();
        for start in 0..=text.len() {
            for end in start..=text.len() {
                for anchored in [false, true] {
                    let input = RegexInput::new(text).range(start..end).anchored(anchored);
                    let reference = oracle::RegexInput::new(text)
                        .range(start..end)
                        .anchored(anchored);
                    let context = format!("{pattern} {start}..{end} anchored={anchored}");
                    assert_eq!(
                        result(
                            regex
                                .find_input(input.clone())
                                .map(|m| m.map(|m| m.range()))
                        ),
                        result(
                            oracle
                                .find_input(reference.clone())
                                .map(|m| m.map(|m| m.range()))
                        ),
                        "{context}"
                    );
                    assert_eq!(
                        result(regex.is_match_input(input.clone())),
                        result(oracle.find_input(reference.clone()).map(|m| m.is_some())),
                        "{context}"
                    );
                    let captures = regex.captures_input(input.clone()).map(|c| {
                        c.map(|c| c.iter().map(|m| m.map(|m| m.range())).collect::<Vec<_>>())
                    });
                    let expected = oracle.captures_input(reference.clone()).map(|c| {
                        c.map(|c| c.iter().map(|m| m.map(|m| m.range())).collect::<Vec<_>>())
                    });
                    assert_eq!(result(captures), result(expected), "{context}");
                }
            }
            let input = RegexInput::new(text).from_pos(start);
            let found: Result<Vec<_>, _> = regex
                .find_iter_input(input)
                .map(|m| m.map(|m| m.range()))
                .collect();
            let expected: Result<Vec<_>, _> = oracle
                .find_iter_input(oracle::RegexInput::new(text).from_pos(start))
                .map(|m| m.map(|m| m.range()))
                .collect();
            assert_eq!(result(found), result(expected), "{pattern} from {start}");
        }
    }
}

/// Byte haystacks, including invalid UTF-8, in every bytes mode: ASCII mode
/// uses automata, the other modes keep the VM for byte input.
#[test]
fn byte_haystacks_equal_the_oracle() {
    let haystacks: [&[u8]; 6] = [
        b"\xff\xffab",
        b"ab\xffab",
        b"\xc3 a\xff",
        b"a\xc3\xa9 b",
        b"",
        b"fn let\xffab",
    ];
    for (mode, reference_mode) in [
        (BytesMode::Unicode, oracle::BytesMode::Unicode),
        (BytesMode::UnicodeBytes, oracle::BytesMode::UnicodeBytes),
        (BytesMode::Ascii, oracle::BytesMode::Ascii),
    ] {
        for pattern in PATTERNS {
            let regex = RegexBuilder::new(pattern).bytes_mode(mode).build();
            let oracle = oracle::RegexBuilder::new(pattern)
                .bytes_mode(reference_mode)
                .build();
            let (Ok(regex), Ok(oracle)) = (regex, oracle) else {
                continue;
            };
            for haystack in haystacks {
                let found: Result<Vec<_>, _> = regex
                    .find_iter(haystack)
                    .map(|m| m.map(|m| m.range()))
                    .collect();
                // The oracle's VM can step past the end of truncated UTF-8 and
                // panic or report out-of-range spans; regexrs stays inside.
                let expected = std::panic::catch_unwind(|| {
                    oracle
                        .find_iter(haystack)
                        .map(|m| m.map(|m| m.range()))
                        .collect::<Result<Vec<_>, _>>()
                });
                let in_bounds = |spans: &[std::ops::Range<usize>]| {
                    spans.iter().all(|span| span.end <= haystack.len())
                };
                match expected {
                    Ok(Ok(spans)) if !in_bounds(&spans) => {
                        assert!(
                            in_bounds(&found.unwrap()),
                            "{pattern} {mode:?} {haystack:?}"
                        );
                        continue;
                    }
                    Ok(expected) => {
                        assert_eq!(
                            result(found),
                            result(expected),
                            "{pattern} {mode:?} {haystack:?}"
                        )
                    }
                    Err(_) => {
                        assert!(
                            in_bounds(&found.unwrap()),
                            "{pattern} {mode:?} {haystack:?}"
                        );
                        continue;
                    }
                }
                assert_eq!(
                    result(regex.is_match(haystack)),
                    result(oracle.is_match(haystack)),
                    "{pattern} {mode:?} {haystack:?}"
                );
            }
        }
    }
}

/// One searcher and one set of capture locations, alternating between the
/// automata (text from a boundary) and the VM (a start inside a character),
/// across hits and misses, keep every result equal to fresh searches.
#[test]
fn retained_storage_alternates_routes_without_contamination() {
    let regex = Regex::new(r"\b(\w)(\w)?\b").unwrap();
    let mut search = regex.searcher();
    let mut locations = regex.capture_locations();
    let inputs = [
        RegexInput::new("é ab"),
        RegexInput::new("é ab").from_pos(1),
        RegexInput::new("--"),
        RegexInput::new("x é"),
        RegexInput::new("é é").range(1..3),
        RegexInput::new(""),
        RegexInput::new("ab cd"),
    ];
    for _ in 0..2 {
        for input in inputs.iter().cloned() {
            let expected = regex.captures_input(input.clone()).unwrap();
            let spans = |c: &regexrs::Captures<'_>| -> Spans {
                c.iter().map(|m| m.map(|m| (m.start(), m.end()))).collect()
            };
            assert_eq!(
                search
                    .captures_input(input.clone())
                    .unwrap()
                    .as_ref()
                    .map(spans),
                expected.as_ref().map(spans)
            );
            let found = search
                .captures_read_input(&mut locations, input.clone())
                .unwrap();
            assert_eq!(
                found.map(|m| m.range()),
                expected.as_ref().map(|c| c.get(0).unwrap().range())
            );
            let read: Spans = (0..locations.len()).map(|i| locations.get(i)).collect();
            assert_eq!(read, expected.as_ref().map_or(vec![None; 3], spans));
            assert_eq!(
                search.is_match_input(input.clone()).unwrap(),
                expected.is_some()
            );
            let iterated: Vec<_> = search
                .find_iter_input(input.clone())
                .map(|m| m.unwrap().range())
                .collect();
            let fresh: Vec<_> = regex
                .find_iter_input(input)
                .map(|m| m.unwrap().range())
                .collect();
            assert_eq!(iterated, fresh);
        }
    }
}

/// By default the automata route cannot exceed a backtrack limit, so a
/// regular pattern that makes the VM explode gives its correct answer. An
/// explicit limit keeps the VM and its exact limit errors, as the oracle has.
#[test]
fn explicit_limits_keep_vm_accounting() {
    let pattern = r"\b(?:a|a)+\b!";
    let text = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let default = Regex::new(pattern).unwrap();
    assert_eq!(default.find(text).unwrap(), None);
    assert!(!default.is_match(text).unwrap());
    let oracle = oracle::Regex::new(pattern).unwrap();
    assert!(oracle.find(text).is_err());
    let explicit = RegexBuilder::new(pattern)
        .backtrack_limit(1_000_000)
        .build()
        .unwrap();
    assert!(matches!(
        explicit.find(text),
        Err(Error::RuntimeError(RuntimeError::BacktrackLimitExceeded))
    ));
    assert!(explicit.is_match(text).is_err());
}

/// Where the lazy DFA gives up on non-ASCII text, the VM searches; if it
/// exceeds the default limit, the automata answer instead. A search the route
/// does not apply to, such as one starting inside a character, keeps the VM's
/// error.
#[test]
fn default_limit_errors_fall_back_to_automata_on_non_ascii_text() {
    let regex = Regex::new(r"\b(?:a|a)+\b!").unwrap();
    let a = "a".repeat(30);
    // The DFA reaches the non-ASCII character after the run of a's.
    let miss = format!("{a}é");
    let hit = format!("{a}é a!");
    let at = hit.len() - 2;
    assert_eq!(regex.find(&miss).unwrap(), None);
    assert!(!regex.is_match(&miss).unwrap());
    assert_eq!(
        regex.find(&hit).unwrap().map(|m| m.range()),
        Some(at..hit.len())
    );
    assert!(regex.is_match(&hit).unwrap());
    let captures = regex.captures(&hit).unwrap().unwrap();
    assert_eq!(captures.get(0).map(|m| m.range()), Some(at..hit.len()));
    let mut search = regex.searcher();
    let mut locations = regex.capture_locations();
    for text in [&hit, &miss, &hit] {
        let expected = regex.find(text).unwrap().map(|m| m.range());
        assert_eq!(
            search.find(text.as_str()).unwrap().map(|m| m.range()),
            expected
        );
        assert_eq!(search.is_match(text.as_str()).unwrap(), expected.is_some());
        let found = search.captures_read(&mut locations, text.as_str()).unwrap();
        assert_eq!(found.map(|m| m.range()), expected);
        assert_eq!(locations.get(0), expected.map(|r| (r.start, r.end)));
    }
    let oracle = oracle::Regex::new(r"\b(?:a|a)+\b!").unwrap();
    assert!(oracle.find(&hit).is_err());
    let text = format!("é {hit}");
    let interior = RegexInput::new(text.as_str()).from_pos(1);
    assert!(matches!(
        regex.find_input(interior),
        Err(Error::RuntimeError(RuntimeError::BacktrackLimitExceeded))
    ));
}

/// Named groups keep their indices and names on either route.
#[test]
fn named_groups_equal_the_oracle() {
    for pattern in [
        r"\b(?<first>\w)(?<second>\w)?\b",
        r"\b(?P<word>\w+)\b(?:-(?<tail>\w+))?",
    ] {
        let regex = Regex::new(pattern).unwrap();
        let oracle = oracle::Regex::new(pattern).unwrap();
        let names: Vec<_> = regex.capture_names().collect();
        assert_eq!(
            names,
            oracle.capture_names().collect::<Vec<_>>(),
            "{pattern}"
        );
        for text in ["ab c-de", "é ab-é", "x", "--", "fn é-K"] {
            let found: Vec<Vec<_>> = regex
                .captures_iter(text)
                .map(|c| {
                    let c = c.unwrap();
                    names
                        .iter()
                        .flatten()
                        .map(|name| c.name(name).map(|m| m.range()))
                        .collect()
                })
                .collect();
            let expected: Vec<Vec<_>> = oracle
                .captures_iter(text)
                .map(|c| {
                    let c = c.unwrap();
                    names
                        .iter()
                        .flatten()
                        .map(|name| c.name(name).map(|m| m.range()))
                        .collect()
                })
                .collect();
            assert_eq!(found, expected, "{pattern} {text:?}");
        }
    }
}

/// Generated patterns over word boundaries, classes, anchors, lazy and
/// bounded repetition, groups and flags, on text with case folds of
/// different widths: every result equals the oracle, apart from searches
/// where the oracle exceeds its backtrack limit and regexrs answers.
#[test]
fn generated_patterns_equal_the_oracle() {
    struct Rng(u64);
    impl Rng {
        fn below(&mut self, n: usize) -> usize {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            (self.0 % n as u64) as usize
        }
        fn pick<'a>(&mut self, items: &[&'a str]) -> &'a str {
            items[self.below(items.len())]
        }
    }
    fn alternation(rng: &mut Rng, depth: u32) -> String {
        let atoms = [
            "a",
            "b",
            "é",
            r"\w",
            r"\d",
            r"\s",
            ".",
            "[ab]",
            r"\b",
            r"\B",
            r"\<",
            r"\>",
            "k",
            "-",
            r"\W",
            "^",
            "$",
            "s",
            "ß",
            "σ",
            "[^a]",
            "",
            r"\b{start-half}",
            r"\b{end}",
        ];
        let quantifiers = ["", "", "", "*", "+", "?", "{1,2}", "*?", "+?", "??"];
        let mut branches = Vec::new();
        for _ in 0..=rng.below(2) {
            let mut branch = String::new();
            for _ in 0..=rng.below(4) {
                if depth < 2 && rng.below(5) == 0 {
                    let inner = alternation(rng, depth + 1);
                    let group = match rng.below(3) {
                        0 => format!("({inner})"),
                        1 => format!("(?:{inner})"),
                        _ => format!("(?<g{}>{inner})", rng.below(1000)),
                    };
                    branch.push_str(&group);
                } else {
                    branch.push_str(rng.pick(&atoms));
                }
                branch.push_str(rng.pick(&quantifiers));
            }
            branches.push(branch);
        }
        branches.join("|")
    }
    let spans = |c: &regexrs::Captures<'_>| -> Spans {
        c.iter().map(|m| m.map(|m| (m.start(), m.end()))).collect()
    };
    let oracle_spans = |c: &oracle::Captures<'_, str>| -> Spans {
        c.iter().map(|m| m.map(|m| (m.start(), m.end()))).collect()
    };
    let flags = ["", "(?i)", "(?m)", "(?s)", "(?is)", "(?R)", "(?im)", "(?x)"];
    let alphabet = [
        "a", "b", "é", "\u{212A}", " ", "1", "\n", "-", "k", "Ж", "s", "\u{17F}", "ß", "\u{1E9E}",
        "Σ", "σ", "ς", "\r",
    ];
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    for _ in 0..3000 {
        let pattern = format!("{}{}", rng.pick(&flags), alternation(&mut rng, 0));
        let (Ok(regex), Ok(oracle)) = (Regex::new(&pattern), oracle::Regex::new(&pattern)) else {
            continue;
        };
        for _ in 0..8 {
            let text: String = (0..rng.below(9)).map(|_| rng.pick(&alphabet)).collect();
            let text = text.as_str();
            let context = format!("{pattern} {text:?}");
            let found: Result<Vec<_>, _> = regex
                .find_iter(text)
                .map(|m| m.map(|m| m.range()))
                .collect();
            let expected: Result<Vec<_>, _> = oracle
                .find_iter(text)
                .map(|m| m.map(|m| m.range()))
                .collect();
            match (found, expected) {
                (Ok(found), Ok(expected)) => assert_eq!(found, expected, "{context}"),
                (Ok(_) | Err(_), Err(_)) => continue,
                (Err(error), Ok(_)) => panic!("{context}: {error:?}"),
            }
            assert_eq!(
                regex.is_match(text).unwrap(),
                oracle.is_match(text).unwrap(),
                "{context}"
            );
            let captured: Vec<Spans> = regex
                .captures_iter(text)
                .map(|c| spans(&c.unwrap()))
                .collect();
            let expected: Vec<Spans> = oracle
                .captures_iter(text)
                .map(|c| oracle_spans(&c.unwrap()))
                .collect();
            assert_eq!(captured, expected, "{context}");
            let start = rng.below(text.len() + 1);
            if text.is_char_boundary(start) {
                let found = regex
                    .find_input(RegexInput::new(text).from_pos(start))
                    .unwrap();
                let expected = oracle
                    .find_input(oracle::RegexInput::new(text).from_pos(start))
                    .unwrap();
                assert_eq!(
                    found.map(|m| m.range()),
                    expected.map(|m| m.range()),
                    "{context} from {start}"
                );
            }
        }
    }
}

/// Only the default backtrack limit is recovered after the VM ran. Here the
/// DFA gives up on non-ASCII text and the VM overflows its backtrack stack;
/// every API reports that error, as the oracle does, retained capture storage
/// is cleared, and the same searcher and storage then work again.
#[test]
fn vm_stack_overflow_is_reported_not_recovered() {
    let pattern = r"\b\w*\b";
    let text = "é".repeat(1_000_001);
    let regex = Regex::new(pattern).unwrap();
    let overflow = |e: &Error| matches!(e, Error::RuntimeError(RuntimeError::StackOverflow));
    let oracle = oracle::Regex::new(pattern).unwrap();
    assert!(oracle.find(&text).is_err());
    assert!(overflow(&regex.find(&text).unwrap_err()));
    assert!(overflow(&regex.is_match(&text).unwrap_err()));
    assert!(overflow(&regex.captures(&text).unwrap_err()));
    let mut search = regex.searcher();
    let mut locations = regex.capture_locations();
    assert!(
        search
            .captures_read(&mut locations, "ab cd")
            .unwrap()
            .is_some()
    );
    assert!(overflow(
        &search
            .captures_read(&mut locations, text.as_str())
            .unwrap_err()
    ));
    assert_eq!(locations.get(0), None);
    assert!(overflow(&search.find(text.as_str()).unwrap_err()));
    assert!(overflow(&search.is_match(text.as_str()).unwrap_err()));
    assert!(overflow(&search.captures(text.as_str()).unwrap_err()));
    let found = search.captures_read(&mut locations, "é ab").unwrap();
    assert_eq!(found.map(|m| m.range()), Some(0..2));
    assert_eq!(locations.get(0), Some((0, 2)));
}
