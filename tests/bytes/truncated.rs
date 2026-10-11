//! Byte haystacks that end with a truncated UTF-8 sequence. Outside ASCII mode
//! the VM steps over such a sequence by the length its first byte implies,
//! past the end of the haystack. Instructions that read nothing still succeed
//! there, so a look-around can return in bounds with a result; those results
//! must equal the oracle. Where the oracle indexes out of bounds and panics,
//! the reading instruction fails instead.

use regexrs::{BytesMode, RegexBuilder};

type Spans = Vec<Option<(usize, usize)>>;

const MODES: [(BytesMode, oracle::BytesMode); 3] = [
    (BytesMode::Unicode, oracle::BytesMode::Unicode),
    (BytesMode::UnicodeBytes, oracle::BytesMode::UnicodeBytes),
    (BytesMode::Ascii, oracle::BytesMode::Ascii),
];

const HAYSTACKS: [&[u8]; 8] = [
    b"\xFF",
    b"\xC3",
    b"\xF0\x9F",
    b"\xC3 a\xFF",
    b"a\xE2\x82",
    b"ab\xFFab",
    b"",
    b"ab",
];

/// Patterns that step over the last character inside a look-around and then
/// test something there: nothing, an assertion, a step back, a delegate, a
/// capture or a backreference, for each kind of `.` and both polarities.
fn patterns() -> Vec<String> {
    let mut patterns = Vec::new();
    for dot in ["(?s:.)", "(?-s:.)", "(?R:.)", "."] {
        for open in ["(?=", "(?!"] {
            patterns.push(format!("{open}(?=){dot}(?=))(a?)\\1"));
            patterns.push(format!("{open}{dot}(?=))(a?)\\1"));
            patterns.push(format!("{open}{dot})"));
            for after in [
                r"\b", r"\B", "$", r"\z", r"\Z", "(?<=a)", r"\<", "(?:ab|c)", "[ab]+", r"\K",
            ] {
                patterns.push(format!("{open}{dot}{after})x?"));
            }
            patterns.push(format!("(?m){open}{dot}$)"));
            patterns.push(format!("(?m){open}{dot}^)"));
            patterns.push(format!("{open}({dot}))"));
            patterns.push(format!("{open}(?=)({dot})(?=))"));
            patterns.push(format!("{open}({dot})(?=))\\1"));
            patterns.push(format!("{open}(?=){dot}(?=)())"));
        }
    }
    for pattern in [
        r"(?=\B.(?=))\B",
        r"(?!\B.(?=))\B",
        r"(?=\B.\K)\B",
        r"((?=.))\1",
        r"(?!.)(a?)\1",
        r"(?s)\b.\b",
        r"(?s)(?>.)\b",
        r"(?=(?~x)(?=))(a?)\1",
        r"(?!(?~x)(?=))(a?)\1",
        r"(?=(?~x)\b)",
        r"(?=(?~x)$)",
        r"(?=(?~x)(?:ab|c))",
        r"(?~x)\b",
        r"(?~x)",
        r"(?=(?=)(?s:.)(?=))\G",
    ] {
        patterns.push(pattern.into());
    }
    patterns
}

#[derive(Debug, PartialEq)]
struct Observed {
    find: Option<std::ops::Range<usize>>,
    captures: Option<Spans>,
    all: Vec<std::ops::Range<usize>>,
    is_match: bool,
}

macro_rules! observe {
    ($regex:expr, $haystack:expr) => {{
        let haystack: &[u8] = $haystack;
        Observed {
            find: $regex.find(haystack).unwrap().map(|m| m.range()),
            captures: $regex.captures(haystack).unwrap().map(|c| {
                (0..c.len())
                    .map(|i| c.get(i).map(|m| (m.start(), m.end())))
                    .collect()
            }),
            all: $regex
                .find_iter(haystack)
                .map(|m| m.unwrap().range())
                .collect(),
            is_match: $regex.is_match(haystack).unwrap(),
        }
    }};
}

#[test]
fn look_arounds_past_a_truncated_tail_equal_the_oracle() {
    let quiet = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let (mut compared, mut oracle_panics) = (0, 0);
    for pattern in patterns() {
        for (mode, reference_mode) in MODES {
            let regex = RegexBuilder::new(&pattern).bytes_mode(mode).build();
            let reference = oracle::RegexBuilder::new(&pattern)
                .bytes_mode(reference_mode)
                .build();
            let (Ok(regex), Ok(reference)) = (regex, reference) else {
                continue;
            };
            for haystack in HAYSTACKS {
                let context = format!("{pattern:?} {mode:?} {haystack:?}");
                let found = std::panic::catch_unwind(|| observe!(regex, haystack));
                let Ok(found) = found else {
                    std::panic::set_hook(quiet);
                    panic!("regexrs panicked: {context}");
                };
                match std::panic::catch_unwind(|| observe!(reference, haystack)) {
                    Ok(expected) => {
                        assert_eq!(found, expected, "{context}");
                        compared += 1;
                    }
                    // The oracle indexes out of bounds; the match itself
                    // must still lie in the haystack.
                    Err(_) => {
                        let ends = found.find.iter().chain(&found.all).map(|m| m.end);
                        assert!(
                            ends.into_iter().all(|end| end <= haystack.len()),
                            "{context}"
                        );
                        oracle_panics += 1;
                    }
                }
            }
        }
    }
    std::panic::set_hook(quiet);
    assert!(
        compared > 2500 && oracle_panics > 50,
        "{compared} {oracle_panics}"
    );
}

/// The cases the acceptance probe reported, with the results of the published
/// 0.1.3 baseline: a look-ahead whose `.` steps past a truncated tail still
/// succeeds after a look-ahead that reads nothing.
#[test]
fn look_ahead_over_a_truncated_tail_keeps_its_result() {
    let both: Spans = vec![Some((0, 0)), Some((0, 0))];
    for mode in [BytesMode::Unicode, BytesMode::UnicodeBytes] {
        let positive = RegexBuilder::new(r"(?=(?=).(?=))(a?)\1")
            .bytes_mode(mode)
            .build()
            .unwrap();
        let negative = RegexBuilder::new(r"(?!(?=).(?=))(a?)\1")
            .bytes_mode(mode)
            .build()
            .unwrap();
        for haystack in [&b"\xFF"[..], b"\xC3"] {
            assert_eq!(observe!(positive, haystack).find, Some(0..0), "{mode:?}");
            assert_eq!(observe!(positive, haystack).captures, Some(both.clone()));
            assert_eq!(observe!(negative, haystack).find, None, "{mode:?}");
            assert_eq!(observe!(negative, haystack).captures, None);
        }
        assert_eq!(
            observe!(negative, b"\xC3 a\xFF").all,
            Vec::<std::ops::Range<usize>>::new(),
            "{mode:?}"
        );
    }
    let ascii = RegexBuilder::new(r"(?!(?=).(?=))(a?)\1")
        .bytes_mode(BytesMode::Ascii)
        .build()
        .unwrap();
    assert_eq!(observe!(ascii, b"\xC3 a\xFF").find, Some(4..4));
}
