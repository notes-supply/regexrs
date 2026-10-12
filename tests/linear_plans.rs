//! Plans search patterns whose matches follow one deterministic path, which
//! the next character decides, without the meta engine or the word-boundary
//! route's automata. Every search API must give the regex crate's answers.
//! The counterexamples are shapes where a maximal or minimal run, choosing an
//! alternative or repetition by the next character, or skipping past a
//! failed run would differ from leftmost-first matching, or where attempts
//! could rescan unbounded text, so they must keep an engine.

use regex_automata::meta::Regex as Meta;
use regex_automata::{Anchored, Input};
use regexrs::{BytesMode, Error, Regex, RegexBuilder, RegexInput, RegularRegex, RuntimeError};

type Groups = Vec<Option<(usize, usize)>>;

const PATTERNS: &[&str] = &[
    // Class runs and delimited fields.
    r"\w+",
    "[a-z]+",
    r"\d+",
    r#""[^"]*""#,
    r#""([^"]*)""#,
    r"[^\r\n]+",
    ".+",
    "(?s).+",
    "é+",
    "[а-я]+",
    r"\p{Greek}+",
    // Literal-led sequences, captures and bounded steps.
    "Sherlock Holmes",
    "(?i)sherlock",
    r"\r\n",
    r"\bfn\s+(\w+)",
    r"(?i)\bfn\s+(\w+)",
    r"\b(?<word>\w+)\b",
    r"(\w+)@(\w+)",
    r"(?<key>\w+):\s*(?<value>\w+)",
    r"\d{4}-\d{2}-\d{2}",
    "[A-Z][a-z]+",
    "a{1,3}b*c",
    // Bounded first runs inside longer runs of their class.
    "[0-9a-f]{8}-[0-9a-f]{4}",
    r"\d{2,4}-\d",
    r"\b[a-z]{2,4}\d",
    r"[a-z]{2,3}\b",
    "[ab]{3}c",
    // Inside the run, a later start passes the assertion an earlier one fails.
    r"(?m)^[a\n]{1,2}b",
    r"(?m)^[a\n]{1,3}b",
    "a+b",
    r"x\d*y",
    "()a",
    "(a)()b",
    // Lazy runs: minimal where nothing can fail after them, else maximal.
    r"\w+?",
    r"a\w*?",
    r"^a\w*?$",
    r"\w+?\b",
    // Edge assertions.
    r"^\w+$",
    r"(?m)^\w+$",
    r"(?m)\w+$",
    r"\A\w+",
    r"\b\w+\b",
    r"[a-z]+\b",
    r"\d+\b",
    // Ordered alternatives the next character chooses between.
    "Holmes|Watson|Lestrade",
    r"\b(?:fn|let|mut|impl|struct|pub)\b",
    "(?i)holmes|watson",
    "(a)|(b)c|(d)",
    "x(?:y|(z))w",
    // Alternatives whose first characters share a UTF-8 lead byte.
    "(é)|(ê)x",
    // Finite literal sets in any order of preference, searched for directly.
    "ab|ac",
    "a|ab",
    "ab|a",
    "foo|f",
    "(?:a|)b",
    // Optional and repeated groups, greedy and lazy, with captures.
    "colou?r",
    "(a)?(b*)",
    r"(\d+)(?:\.(\d+))?",
    r"\w+(?:\.\w+)*",
    "(?:ab)+",
    "(?:ab)+?",
    "(?:ab){2,3}",
    "x(?:ab)*?y",
    "(?:(a)|(b))+",
    "(?:a(b)?)+",
    "((?:ab)*)(c?)",
    r"(?:\.(\w))*",
    "(?:foo|bar)(?:baz)?",
    r"(?:-(\d+)){1,3}",
    // Patterns that can match nothing.
    r"\w*",
    "(?:ab)*",
    "a?",
    // Counterexamples, which must not take a plan.
    r"\w+\B",
    r"\w+s",
    r"(?m)[^a]+$",
    "[a-z]+?x",
    r"\d+$",
    "ab+|ac",
    "(a|ab)c",
    "(?:ab)*a",
    "(?:a|)b+",
    // Attempts could rescan unbounded repetitions holding other starts.
    "(?:a(b)?)+c",
    r"\b(?:ab)+\b",
];

/// The patterns above that must take a plan, through `Regex` and
/// `RegularRegex` alike; the counterexamples after them must not.
const PLANNED: usize = PATTERNS.len() - 11;

const TEXTS: &[&str] = &[
    "",
    "a",
    "abc def",
    "abc ",
    "cats dogs",
    "b\nca",
    "abxcx",
    "abcD xyz",
    "12a 34",
    "x123y x1z x9y",
    "aaac aab",
    "abc d",
    r#""a" "" "b"#,
    "\"é\" \"x",
    "fn main() { fn  foo_1(x) } fn\tbar fn",
    "é fn ünï fn  x é",
    "SHERLOCK Sherlock ſherlock sherloc",
    "Sherlock Holmes and Sherlock  Holmes",
    "2026-10-11 and 1999-01-02x 12-34-56",
    "foo@bar x@y @z",
    "key: value, k:v kk :x",
    "line1\r\nline2\nline3\r\n",
    "café naïve façade €5 𝄞x",
    "Привет мир, як справи? тест",
    "αβγ Δ λόγος",
    "aaabbbc aabbc abc ac bc",
    "abababababababababababababababababababab",
    "deadbeefcafe-1234 0123456789abcdef-abcd 12-3 1234567-8 ab1 abcde1 x abc1",
    "aaaabc abababc bbbc abc ababc",
    "a\n\nb x\na\nb",
    "aa\n\nb",
    "colour color colouur",
    "1.2 3. 4.5.6 .7 8..9",
    "a.b.c a. .b x.y.",
    "abababx abx ab aba",
    "xababy xy xaby xabay",
    "foobaz barbaz foo bar baz fooba",
    "-1-2-3-4 -5 - -6-",
    "Holmes Watson Lestrade Holm HOLMES watson",
    "let mut x; fn impl pub structs pub(crate)",
    "aabacb abcabc bc ac",
    "xyw xzw xw xyzw",
    "é êx ê ex éêx",
];

fn oracle_captures(
    meta: &Meta,
    text: &str,
    span: (usize, usize),
    anchored: bool,
) -> Option<Groups> {
    let input = Input::new(text).span(span.0..span.1).anchored(if anchored {
        Anchored::Yes
    } else {
        Anchored::No
    });
    let mut caps = meta.create_captures();
    meta.search_captures(&input, &mut caps);
    caps.is_match().then(|| {
        (0..caps.group_len())
            .map(|i| caps.get_group(i).map(|s| (s.start, s.end)))
            .collect()
    })
}

fn groups(c: &regexrs::Captures<'_>) -> Groups {
    c.iter().map(|m| m.map(|m| (m.start(), m.end()))).collect()
}

/// Character boundaries to start, end and anchor searches at.
fn points(text: &str) -> Vec<usize> {
    let bounds: Vec<usize> = (0..=text.len())
        .filter(|&i| text.is_char_boundary(i))
        .collect();
    let n = bounds.len();
    let mut points = vec![
        0,
        bounds[1.min(n - 1)],
        bounds[n / 2],
        bounds[n.saturating_sub(2)],
        text.len(),
    ];
    points.sort_unstable();
    points.dedup();
    points
}

/// The comparisons below check the plans themselves, not an engine they
/// could defer to.
#[test]
fn admitted_families_take_plans() {
    for (index, pattern) in PATTERNS.iter().enumerate() {
        let planned = index < PLANNED;
        assert_eq!(
            Regex::new(pattern).unwrap().planned(),
            planned,
            "Regex {pattern:?}"
        );
        assert_eq!(
            RegularRegex::new(pattern).unwrap().planned(),
            planned,
            "RegularRegex {pattern:?}"
        );
    }
}

#[test]
fn plans_equal_the_regex_crate_across_apis() {
    for pattern in PATTERNS {
        let oracle = regex::Regex::new(pattern).unwrap();
        let meta = Meta::new(pattern).unwrap();
        let ordinary = Regex::new(pattern).unwrap();
        let regular = RegularRegex::new(pattern).unwrap();
        let word_boundaries = pattern.contains(r"\b") || pattern.contains(r"\B");
        for &text in TEXTS {
            let context = format!("{pattern:?} {text:?}");
            let expected: Vec<Groups> = oracle
                .captures_iter(text)
                .map(|c| c.iter().map(|m| m.map(|m| (m.start(), m.end()))).collect())
                .collect();
            let found: Vec<Groups> = ordinary
                .captures_iter(text)
                .map(|c| groups(&c.unwrap()))
                .collect();
            assert_eq!(found, expected, "Regex::captures_iter {context}");
            let found: Vec<Groups> = regular.captures_iter(text).map(|c| groups(&c)).collect();
            assert_eq!(found, expected, "RegularRegex::captures_iter {context}");
            let spans: Vec<(usize, usize)> = expected.iter().map(|g| g[0].unwrap()).collect();
            let found: Vec<(usize, usize)> = ordinary
                .find_iter(text)
                .map(|m| m.map(|m| (m.start(), m.end())).unwrap())
                .collect();
            assert_eq!(found, spans, "Regex::find_iter {context}");
            assert_eq!(
                ordinary.is_match(text).unwrap(),
                oracle.is_match(text),
                "{context}"
            );
            assert_eq!(regular.is_match(text), oracle.is_match(text), "{context}");
            let points = points(text);
            for (i, &start) in points.iter().enumerate() {
                for &end in &points[i..] {
                    for anchored in [false, true] {
                        let expected = oracle_captures(&meta, text, (start, end), anchored);
                        let input = || RegexInput::new(text).range(start..end).anchored(anchored);
                        let at = format!("{context} {start}..{end} anchored={anchored}");
                        let found = regular.captures_input(input()).map(|c| groups(&c));
                        assert_eq!(found, expected, "RegularRegex::captures_input {at}");
                        let span = expected.as_ref().map(|g| g[0].unwrap());
                        assert_eq!(regular.is_match_input(input()), span.is_some(), "{at}");
                        // Word boundaries keep the VM's own answers for a range
                        // that ends before the haystack does, as in 0.1.5.
                        if word_boundaries && end < text.len() {
                            continue;
                        }
                        let found = ordinary
                            .captures_input(input())
                            .unwrap()
                            .map(|c| groups(&c));
                        assert_eq!(found, expected, "Regex::captures_input {at}");
                        let found = ordinary
                            .find_input(input())
                            .unwrap()
                            .map(|m| (m.start(), m.end()));
                        assert_eq!(found, span, "Regex::find_input {at}");
                        assert_eq!(
                            ordinary.is_match_input(input()).unwrap(),
                            span.is_some(),
                            "Regex::is_match_input {at}"
                        );
                    }
                }
                // A later start keeps the text before it for assertions.
                let expected = oracle.find_at(text, start).map(|m| (m.start(), m.end()));
                let found = ordinary
                    .find_from_pos(text, start)
                    .unwrap()
                    .map(|m| (m.start(), m.end()));
                assert_eq!(found, expected, "Regex::find_from_pos {context} {start}");
            }
            let replaced = oracle.replace_all(text, "<$0>");
            assert_eq!(ordinary.replace_all(text, "<$0>"), replaced, "{context}");
            assert_eq!(regular.replace_all(text, "<$0>"), replaced, "{context}");
        }
    }
}

/// One searcher and one set of capture locations kept across hits and
/// misses report what fresh searches report.
#[test]
fn retained_storage_follows_plans() {
    for pattern in PATTERNS {
        let regex = Regex::new(pattern).unwrap();
        let mut search = regex.searcher();
        let mut locations = regex.capture_locations();
        for &text in TEXTS.iter().chain(TEXTS.iter().rev()) {
            let context = format!("{pattern:?} {text:?}");
            let expected = regex.captures(text).unwrap().map(|c| groups(&c));
            let found = search.captures_read(&mut locations, text).unwrap();
            assert_eq!(
                found.map(|m| (m.start(), m.end())),
                expected.as_ref().map(|g| g[0].unwrap()),
                "{context}"
            );
            let read: Groups = (0..locations.len()).map(|i| locations.get(i)).collect();
            let empty = vec![None; locations.len()];
            assert_eq!(&read, expected.as_ref().unwrap_or(&empty), "{context}");
            assert_eq!(
                search.is_match(text).unwrap(),
                expected.is_some(),
                "{context}"
            );
            let spans: Vec<(usize, usize)> = search
                .find_iter(text)
                .map(|m| m.map(|m| (m.start(), m.end())).unwrap())
                .collect();
            let fresh: Vec<(usize, usize)> = regex
                .find_iter(text)
                .map(|m| m.map(|m| (m.start(), m.end())).unwrap())
                .collect();
            assert_eq!(spans, fresh, "{context}");
        }
    }
}

/// Byte haystacks with invalid UTF-8: classes match whole characters only in
/// Unicode modes, and single bytes in ASCII mode.
#[test]
fn plans_follow_byte_modes() {
    let texts: [&[u8]; 4] = [
        b"ab\xFFcd",
        b"\xC3\xA9\xC3",
        b"12\x80 34",
        b"\"q\xFF\" \"\"",
    ];
    for (pattern, unicode) in [
        ("[a-z]+", false),
        (r"\d+", false),
        (r#""[^"]*""#, false),
        (r"\w+", true),
        (r#""[^"]*""#, true),
        ("é+", true),
    ] {
        let mode = if unicode {
            BytesMode::UnicodeBytes
        } else {
            BytesMode::Ascii
        };
        let regex = RegexBuilder::new(pattern).bytes_mode(mode).build().unwrap();
        let regular = RegexBuilder::new(pattern)
            .bytes_mode(mode)
            .build_regular()
            .unwrap();
        let oracle = regex::bytes::RegexBuilder::new(pattern)
            .unicode(unicode)
            .build()
            .unwrap();
        for text in texts {
            let expected: Vec<(usize, usize)> = oracle
                .find_iter(text)
                .map(|m| (m.start(), m.end()))
                .collect();
            let found: Vec<(usize, usize)> = regex
                .find_iter(text)
                .map(|m| m.map(|m| (m.start(), m.end())).unwrap())
                .collect();
            assert_eq!(found, expected, "{pattern:?} {text:?}");
            let found: Vec<(usize, usize)> = regular
                .find_iter(text)
                .map(|m| (m.start(), m.end()))
                .collect();
            assert_eq!(found, expected, "{pattern:?} {text:?}");
        }
    }
}

/// A range that ends inside a character never lets a class run take part of
/// it, whichever way the run is scanned.
#[test]
fn runs_stop_at_a_range_end_inside_a_character() {
    let text = "aé\"";
    for pattern in [r#"[^"]+"#, r"\w+", "(?s).+"] {
        let regex = Regex::new(pattern).unwrap();
        let meta = Meta::new(pattern).unwrap();
        let expected = meta
            .find(Input::new(text).span(0..2))
            .map(|m| (m.start(), m.end()));
        let found = regex
            .find_input(RegexInput::new(text).range(0..2))
            .unwrap()
            .map(|m| (m.start(), m.end()));
        assert_eq!(found, expected, "{pattern:?}");
    }
}

/// An explicit backtrack limit keeps word-boundary patterns on the VM, with
/// its exact error; the default limit answers long non-ASCII runs.
#[test]
fn word_boundary_plans_keep_vm_errors() {
    let limited = RegexBuilder::new(r"\b\w+!")
        .backtrack_limit(1000)
        .build()
        .unwrap();
    for text in ["a".repeat(5000), "é".repeat(5000)] {
        assert!(matches!(
            limited.find(&text),
            Err(Error::RuntimeError(RuntimeError::BacktrackLimitExceeded))
        ));
    }
    let regex = Regex::new(r"\b\w+!").unwrap();
    let text = "é".repeat(100_000);
    assert_eq!(regex.find(&text).unwrap(), None);
    let hit = format!("{text} ab!");
    assert_eq!(
        regex.find(&hit).unwrap().map(|m| m.range()),
        Some(text.len() + 1..hit.len())
    );
}

/// A pattern that can match nothing, on invalid UTF-8 in ASCII mode. The
/// meta engine panicked here inside regex-automata ("reverse search must
/// match if forward search does") before the pattern took a plan; the plan
/// reports the regex crate's match. A corrected byte-mode defect, not a
/// change of parity.
#[test]
fn nullable_plans_answer_where_the_engine_panicked() {
    let text: &[u8] = b"\x80\xC3";
    let builder = || {
        let mut builder = RegexBuilder::new(".*");
        builder.bytes_mode(BytesMode::Ascii);
        builder
    };
    let regex = builder().build().unwrap();
    let regular = builder().build_regular().unwrap();
    assert!(regex.planned() && regular.planned());
    let found: Vec<(usize, usize)> = regex
        .find_iter(text)
        .map(|m| m.map(|m| (m.start(), m.end())).unwrap())
        .collect();
    assert_eq!(found, [(0, 2)]);
    let found: Vec<(usize, usize)> = regular
        .find_iter(text)
        .map(|m| (m.start(), m.end()))
        .collect();
    assert_eq!(found, [(0, 2)]);
}

/// A literal that repeats its own start is searched for directly however
/// long it is: no attempt rereads it from each start inside it.
#[test]
fn long_repeating_literals_take_literal_plans() {
    for unit in ["ab", "aab", "é"] {
        let literal = unit.repeat(40);
        let text = format!("x{}y{}", unit.repeat(100), unit.repeat(39));
        let regex = Regex::new(&literal).unwrap();
        let regular = RegularRegex::new(&literal).unwrap();
        assert!(regex.planned() && regular.planned(), "{unit}");
        let expected: Vec<_> = text
            .match_indices(&*literal)
            .map(|(at, found)| (at, at + found.len()))
            .collect();
        let found: Vec<_> = regex
            .find_iter(&text)
            .map(|m| m.map(|m| (m.start(), m.end())).unwrap())
            .collect();
        assert_eq!(found, expected, "Regex {unit}");
        let found: Vec<_> = regular
            .find_iter(&text)
            .map(|m| (m.start(), m.end()))
            .collect();
        assert_eq!(found, expected, "RegularRegex {unit}");
    }
}

/// A pattern that is a set of literals is searched with its prefilter alone,
/// whose single-byte form tests the byte at the start of an anchored search
/// even where the range ends there.
#[test]
fn literal_plans_stay_inside_the_range() {
    for pattern in ["1", "(?:1)", "ab|cd", "[ab]x"] {
        let regex = Regex::new(pattern).unwrap();
        let regular = RegularRegex::new(pattern).unwrap();
        let meta = Meta::new(pattern).unwrap();
        assert!(regex.planned() && regular.planned(), "{pattern}");
        for text in ["1", "ab", "ax", "1ax"] {
            for end in 0..=text.len() {
                for anchored in [false, true] {
                    let mode = if anchored {
                        Anchored::Yes
                    } else {
                        Anchored::No
                    };
                    let expected = meta
                        .find(Input::new(text).span(0..end).anchored(mode))
                        .map(|m| (m.start(), m.end()));
                    let input = || RegexInput::new(text).range(0..end).anchored(anchored);
                    let at = format!("{pattern} {text} 0..{end} {anchored}");
                    let found = regular.find_input(input()).map(|m| (m.start(), m.end()));
                    assert_eq!(found, expected, "{at}");
                    let found = regex
                        .find_input(input())
                        .unwrap()
                        .map(|m| (m.start(), m.end()));
                    assert_eq!(found, expected, "{at}");
                }
            }
        }
    }
}

/// In byte modes a pattern that can match nothing reports a match that is not
/// empty wherever it ends, as the regex crate's byte API does; the meta engine
/// panicked on several of these. An empty match inside a UTF-8 sequence is
/// still skipped, as the meta engine skips it.
#[test]
fn nullable_byte_plans_skip_only_empty_matches_inside_sequences() {
    let spans = |pattern: &str, text: &[u8]| {
        let regex = RegexBuilder::new(pattern)
            .bytes_mode(BytesMode::Ascii)
            .build()
            .unwrap();
        let regular = RegexBuilder::new(pattern)
            .bytes_mode(BytesMode::Ascii)
            .build_regular()
            .unwrap();
        assert!(regex.planned() && regular.planned());
        let found: Vec<(usize, usize)> = regex
            .find_iter(text)
            .map(|m| m.map(|m| (m.start(), m.end())).unwrap())
            .collect();
        let regular: Vec<(usize, usize)> = regular
            .find_iter(text)
            .map(|m| (m.start(), m.end()))
            .collect();
        assert_eq!(found, regular, "{pattern}");
        found
    };
    assert_eq!(
        spans(r"\W?", b"\xE2\x82\xAC\xE2\x82"),
        [(0, 1), (1, 2), (2, 3), (3, 4), (4, 5)]
    );
    assert_eq!(spans(r"\W?", b"\x80\x80a\x80"), [(0, 1), (1, 2), (3, 4)]);
    assert_eq!(spans("a*", b"\x80"), [(1, 1)]);
    assert_eq!(spans("a*", b"\x80a"), [(1, 2)]);
}
