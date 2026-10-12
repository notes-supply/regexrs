//! Generated patterns and texts: plans with alternatives and optional or
//! repeated groups must report the regex crate's matches and captures through
//! every search API, and patterns the plans leave to an engine must too. The
//! generator favours the shapes the plans take but also produces ties between
//! alternatives, repetitions whose next character cannot decide, nullable
//! repeated groups, greedy and lazy mixes, case folding across widths, word
//! boundaries and line anchors. Only the interfaces a plan serves are compared
//! here, so a deferral cannot hide a wrong plan; each run checks that a good
//! share of the patterns was planned. Searches the plans leave to other
//! engines are compared with the frozen regular-pass source instead, which
//! keeps their inherited differences from the regex crate visible there.

use regex_automata::meta::Regex as Meta;
use regex_automata::{Anchored, Input};
use regexrs::{BytesMode, Regex, RegexBuilder, RegexInput, RegularRegex};

type Groups = Vec<Option<(usize, usize)>>;

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

const LITERALS: &[&str] = &["a", "b", "c", "x", "é", "-", r"\.", "k", "1"];
const CLASSES: &[&str] = &[
    "[ab]", "[^a]", r"\d", r"\w", "[a-c]", "é", "(?i:k)", "[^ab]", "[é-ê]", ".", r"\s", "[x-]",
];
const QUANTIFIERS: &[&str] = &[
    "", "", "", "", "?", "*", "+", "??", "*?", "+?", "{1,3}", "{2}", "{0,2}?", "{2,}",
];

/// A random regular pattern; `ascii` keeps it to ASCII literals. Half of
/// them are built from alternatives with distinct first characters, the
/// shape plans take, and the rest from the general grammar.
fn pattern(rng: &mut Rng, ascii: bool) -> String {
    let mut out = String::new();
    if rng.below(10) == 0 {
        out.push_str("(?i)");
    }
    out.push_str(rng.pick(&["", "", "", "", "", "^", r"\b", "(?m)^"]));
    if rng.below(2) == 0 {
        decided(rng, &mut out, 0, ascii);
    } else {
        sequence(rng, &mut out, 0, ascii);
    }
    out.push_str(rng.pick(&["", "", "", "", "", "$", r"\b", "(?m)$"]));
    out
}

/// A sequence whose alternatives start with distinct literals.
fn decided(rng: &mut Rng, out: &mut String, depth: usize, ascii: bool) {
    for _ in 0..1 + rng.below(3) {
        match rng.below(if depth >= 2 { 2 } else { 4 }) {
            0 => out.push_str(rng.pick(&["a", "b", "x", "-", "k1", "ab", "é"])),
            1 => {
                out.push_str(rng.pick(CLASSES));
                out.push_str(rng.pick(&["", "", "+", "*", "?", "{2}", "+?"]));
            }
            kind => {
                out.push_str(if kind == 2 { "(" } else { "(?:" });
                let mut firsts = vec!["a", "b", "x", "-", "1", "é"];
                for branch in 0..1 + rng.below(3) {
                    if branch > 0 {
                        out.push('|');
                    }
                    let first = firsts.remove(rng.below(firsts.len()));
                    out.push_str(if ascii && first == "é" { "e" } else { first });
                    if rng.below(2) == 0 {
                        decided(rng, out, depth + 1, ascii);
                    }
                }
                out.push(')');
                out.push_str(rng.pick(&["", "?", "*", "+", "??", "{1,3}", "*?", "{2}"]));
            }
        }
    }
}

fn sequence(rng: &mut Rng, out: &mut String, depth: usize, ascii: bool) {
    for _ in 0..1 + rng.below(3) {
        atom(rng, out, depth, ascii);
        out.push_str(rng.pick(QUANTIFIERS));
    }
}

fn atom(rng: &mut Rng, out: &mut String, depth: usize, ascii: bool) {
    let choice = if depth >= 2 {
        rng.below(2)
    } else {
        rng.below(6)
    };
    match choice {
        0 => {
            for _ in 0..1 + rng.below(3) {
                let literal = rng.pick(LITERALS);
                out.push_str(if ascii && literal == "é" {
                    "e"
                } else {
                    literal
                });
            }
        }
        1 => {
            let class = rng.pick(CLASSES);
            out.push_str(if ascii && class.contains('é') {
                "[e-f]"
            } else {
                class
            });
        }
        2 => {
            out.push('(');
            sequence(rng, out, depth + 1, ascii);
            out.push(')');
        }
        open => {
            out.push_str(if open == 3 { "(?:" } else { "(" });
            for branch in 0..2 + rng.below(2) {
                if branch > 0 {
                    out.push('|');
                }
                sequence(rng, out, depth + 1, ascii);
            }
            out.push(')');
        }
    }
}

fn text(rng: &mut Rng) -> String {
    const PIECES: &[&str] = &[
        "a", "b", "c", "x", "é", "ê", "-", ".", " ", "k", "K", "\u{212A}", "\n", "1", "ab", "ba",
    ];
    (0..rng.below(14)).map(|_| rng.pick(PIECES)).collect()
}

fn groups(c: &regexrs::Captures<'_>) -> Groups {
    c.iter().map(|m| m.map(|m| (m.start(), m.end()))).collect()
}

fn byte_groups(c: &regexrs::Captures<'_, [u8]>) -> Groups {
    (0..c.len())
        .map(|i| c.get(i).map(|m| (m.start(), m.end())))
        .collect()
}

fn oracle_groups(caps: &regex::Captures<'_>) -> Groups {
    caps.iter()
        .map(|m| m.map(|m| (m.start(), m.end())))
        .collect()
}

fn meta_captures(
    meta: &Meta,
    text: &str,
    start: usize,
    end: usize,
    anchored: bool,
) -> Option<Groups> {
    let anchored = if anchored {
        Anchored::Yes
    } else {
        Anchored::No
    };
    let input = Input::new(text).span(start..end).anchored(anchored);
    let mut caps = meta.create_captures();
    meta.search_captures(&input, &mut caps);
    caps.is_match().then(|| {
        (0..caps.group_len())
            .map(|i| caps.get_group(i).map(|s| (s.start, s.end)))
            .collect()
    })
}

#[test]
fn generated_patterns_equal_the_regex_crate() {
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let (mut compared, mut planned_regular, mut planned_regex) = (0, 0, 0);
    let texts: Vec<String> = (0..12).map(|_| text(&mut rng)).collect();
    for _ in 0..2500 {
        let pattern = pattern(&mut rng, false);
        let Ok(oracle) = regex::Regex::new(&pattern) else {
            continue;
        };
        let meta = Meta::new(&pattern).unwrap();
        let ordinary = Regex::new(&pattern).unwrap_or_else(|e| panic!("{pattern:?}: {e}"));
        let regular = RegularRegex::new(&pattern).unwrap_or_else(|e| panic!("{pattern:?}: {e}"));
        compared += 1;
        let (regular_planned, regex_planned) = (regular.planned(), ordinary.planned());
        planned_regular += usize::from(regular_planned);
        planned_regex += usize::from(regex_planned);
        if !regular_planned && !regex_planned {
            continue;
        }
        let word_boundaries = pattern.contains(r"\b");
        let mut search = ordinary.searcher();
        let mut locations = ordinary.capture_locations();
        let fresh: Vec<String> = (0..4).map(|_| text(&mut rng)).collect();
        for text in texts.iter().chain(&fresh) {
            let text = text.as_str();
            let context = format!("{pattern:?} {text:?}");
            let expected: Vec<Groups> = oracle
                .captures_iter(text)
                .map(|c| oracle_groups(&c))
                .collect();
            let spans: Vec<(usize, usize)> = expected.iter().map(|g| g[0].unwrap()).collect();
            let first = oracle.captures(text).map(|c| oracle_groups(&c));
            if regular_planned {
                let found: Vec<Groups> = regular.captures_iter(text).map(|c| groups(&c)).collect();
                assert_eq!(found, expected, "RegularRegex::captures_iter {context}");
                assert_eq!(regular.is_match(text), oracle.is_match(text), "{context}");
                assert_eq!(
                    regular.replace_all(text, "<$0>"),
                    oracle.replace_all(text, "<$0>"),
                    "{context}"
                );
            }
            if regex_planned {
                let found: Vec<Groups> = ordinary
                    .captures_iter(text)
                    .map(|c| groups(&c.unwrap()))
                    .collect();
                assert_eq!(found, expected, "Regex::captures_iter {context}");
                let found: Vec<(usize, usize)> = search
                    .find_iter(text)
                    .map(|m| m.map(|m| (m.start(), m.end())).unwrap())
                    .collect();
                assert_eq!(found, spans, "Searcher::find_iter {context}");
                assert_eq!(
                    ordinary.is_match(text).unwrap(),
                    oracle.is_match(text),
                    "{context}"
                );
                let found = search.captures_read(&mut locations, text).unwrap();
                assert_eq!(found.is_some(), first.is_some(), "captures_read {context}");
                if let Some(first) = &first {
                    let read: Groups = (0..locations.len()).map(|i| locations.get(i)).collect();
                    assert_eq!(&read, first, "captures_read {context}");
                }
            }
            // Ranges and anchors, including ends inside a character.
            let len = text.len();
            for (start, end) in [
                (0, len),
                (1.min(len), len),
                (0, len / 2),
                (len / 3, len - len / 4),
            ] {
                for anchored in [false, true] {
                    let expected = meta_captures(&meta, text, start, end, anchored);
                    let input = || RegexInput::new(text).range(start..end).anchored(anchored);
                    let at = format!("{context} {start}..{end} anchored={anchored}");
                    let span = expected.as_ref().map(|g| g[0].unwrap());
                    if regular_planned {
                        let found = regular.captures_input(input()).map(|c| groups(&c));
                        assert_eq!(found, expected, "RegularRegex::captures_input {at}");
                        let found = regular.find_input(input()).map(|m| (m.start(), m.end()));
                        assert_eq!(found, span, "RegularRegex::find_input {at}");
                        assert_eq!(regular.is_match_input(input()), span.is_some(), "{at}");
                    }
                    // The VM keeps its own answers for word boundaries in a
                    // range ending early or starting inside a character.
                    let inside = !text.is_char_boundary(start) || !text.is_char_boundary(end);
                    if !regex_planned || word_boundaries && (end < len || inside) {
                        continue;
                    }
                    let found = ordinary
                        .captures_input(input())
                        .unwrap()
                        .map(|c| groups(&c));
                    assert_eq!(found, expected, "Regex::captures_input {at}");
                    assert_eq!(
                        ordinary.is_match_input(input()).unwrap(),
                        span.is_some(),
                        "Regex::is_match_input {at}"
                    );
                }
            }
        }
    }
    eprintln!(
        "compared {compared}, planned: RegularRegex {planned_regular}, Regex {planned_regex}"
    );
    assert!(compared > 2000, "{compared}");
    assert!(
        planned_regular * 4 > compared,
        "{planned_regular} of {compared}"
    );
    assert!(
        planned_regex * 4 > compared,
        "{planned_regex} of {compared}"
    );
}

/// Byte haystacks with invalid UTF-8, in ASCII mode against the regex crate
/// without Unicode, and in Unicode bytes mode against it with Unicode.
#[test]
fn generated_patterns_follow_byte_modes() {
    let mut rng = Rng(0xD1B5_4A32_D192_ED03);
    const BYTES: &[&[u8]] = &[
        b"a",
        b"b",
        b"c",
        b"x",
        b"-",
        b".",
        b" ",
        b"k",
        b"1",
        b"\xFF",
        b"\xC3",
        b"\xC3\xA9",
        b"\x80",
        b"\n",
        b"ab",
    ];
    let (mut planned, mut nullable) = (0, 0);
    for _ in 0..2500 {
        let unicode = rng.below(2) == 0;
        let pattern = pattern(&mut rng, !unicode);
        // Word boundaries stay Unicode's in ASCII mode.
        let oracle_pattern = pattern.replace(r"\b", r"(?u:\b)");
        let Ok(oracle) = regex::bytes::RegexBuilder::new(&oracle_pattern)
            .unicode(unicode)
            .build()
        else {
            continue;
        };
        let mode = if unicode {
            BytesMode::UnicodeBytes
        } else {
            BytesMode::Ascii
        };
        let Ok(regular) = RegexBuilder::new(&pattern).bytes_mode(mode).build_regular() else {
            continue;
        };
        let ordinary = RegexBuilder::new(&pattern)
            .bytes_mode(mode)
            .build()
            .unwrap();
        if !regular.planned() {
            continue;
        }
        planned += 1;
        // Empty matches inside a UTF-8 sequence are not reported, as by the
        // meta engine regexrs configures, even in byte modes; the regex
        // crate's byte API reports them.
        let can_be_empty = regex_syntax::ParserBuilder::new()
            .unicode(unicode)
            .utf8(false)
            .build()
            .parse(&oracle_pattern)
            .is_ok_and(|hir| hir.properties().minimum_len() == Some(0));
        nullable += usize::from(can_be_empty);
        for _ in 0..8 {
            let text: Vec<u8> = (0..rng.below(12))
                .flat_map(|_| BYTES[rng.below(BYTES.len())].iter().copied())
                .collect();
            let context = format!("{pattern:?} {mode:?} {text:?}");
            let inside = |at: usize| text.get(at).is_some_and(|&b| b & 0xC0 == 0x80);
            let expected: Vec<Groups> = oracle
                .captures_iter(&text)
                .map(|c| {
                    c.iter()
                        .map(|m| m.map(|m| (m.start(), m.end())))
                        .collect::<Groups>()
                })
                .filter(|groups| {
                    let (start, end) = groups[0].unwrap();
                    !(can_be_empty && start == end && inside(start))
                })
                .collect();
            let found: Vec<Groups> = regular
                .captures_iter(text.as_slice())
                .map(|c| byte_groups(&c))
                .collect();
            assert_eq!(found, expected, "RegularRegex {context}");
            // Outside ASCII mode, word boundaries on bytes keep the VM, which
            // steps over invalid UTF-8 its own way.
            if ordinary.planned() && !(unicode && pattern.contains(r"\b")) {
                let found: Vec<Groups> = ordinary
                    .captures_iter(text.as_slice())
                    .map(|c| byte_groups(&c.unwrap()))
                    .collect();
                assert_eq!(found, expected, "Regex {context}");
            }
        }
    }
    eprintln!("byte modes: planned {planned}, of which can be empty {nullable}");
    assert!(planned > 300, "{planned}");
}
