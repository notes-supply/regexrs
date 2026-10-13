//! Seek preambles composed with start filtering. Every configuration must give
//! the oracle's matches, captures and, with an explicit backtrack limit, its
//! exact limit errors. Under the default limit a leading lookbehind's literals
//! may lead a seek, which only removes attempts the start filter proves fail.

use oracle::RegexInput as OracleInput;
use regexrs::{Error, Regex, RegexBuilder, RegexInput, RuntimeError, seek_pattern_is_useful};

type Spans = Vec<Option<(usize, usize)>>;

const CONFIGS: [&str; 4] = ["default", "seek", "always", "custom"];

fn build(pattern: &str, config: &str, limit: Option<usize>) -> Regex {
    let mut builder = RegexBuilder::new(pattern);
    match config {
        "seek" => {
            builder.seek(true);
        }
        "always" => {
            builder.seek_filter(|_| true);
        }
        "custom" => {
            builder.seek_filter(|pattern| pattern.len() > 6 && seek_pattern_is_useful(pattern));
        }
        _ => {}
    }
    if let Some(limit) = limit {
        builder.backtrack_limit(limit);
    }
    builder.build().unwrap()
}

fn build_oracle(pattern: &str, config: &str, limit: Option<usize>) -> oracle::Regex {
    let mut builder = oracle::RegexBuilder::new(pattern);
    match config {
        "seek" => {
            builder.seek(true);
        }
        "always" => {
            builder.seek_filter(|_| true);
        }
        "custom" => {
            builder.seek_filter(|pattern| {
                pattern.len() > 6 && oracle::seek_pattern_is_useful(pattern)
            });
        }
        _ => {}
    }
    if let Some(limit) = limit {
        builder.backtrack_limit(limit);
    }
    builder.build().unwrap()
}

/// Every match's groups, then ranged, anchored and replacement results, with
/// errors as their debug text.
macro_rules! observe {
    ($regex:expr, $text:expr, $input:ident) => {{
        let text: &str = $text;
        let mut all: Vec<Result<Spans, String>> = Vec::new();
        for captures in $regex.captures_iter(text) {
            match captures {
                Ok(c) => all.push(Ok(c
                    .iter()
                    .map(|m| m.map(|m| (m.start(), m.end())))
                    .collect())),
                Err(e) => {
                    all.push(Err(format!("{e:?}")));
                    break;
                }
            }
        }
        let end = text.len().saturating_sub(1);
        let start = text.char_indices().nth(1).map_or(0, |(at, _)| at).min(end);
        let end = (start..=end)
            .rev()
            .find(|&at| text.is_char_boundary(at))
            .unwrap_or(start);
        let ranged = $regex
            .find_input($input::new(text).range(start..end))
            .map(|m| m.map(|m| m.range()))
            .map_err(|e| format!("{e:?}"));
        let anchored = $regex
            .find_input($input::new(text).anchored(true))
            .map(|m| m.map(|m| m.range()))
            .map_err(|e| format!("{e:?}"));
        let is_match = $regex.is_match(text).map_err(|e| format!("{e:?}"));
        (all, ranged, anchored, is_match)
    }};
}

const PATTERNS: [&str; 16] = [
    // A leading lookbehind's literals lead the seek under the default limit.
    r"(?<=Mr\. |Dr\. )[A-Z]\w+",
    r"(?<=\.)[a-z][a-z0-9]*(?=\()",
    r"(?<=x)[ab]b(?!c)",
    r"(?<=é#)\d",
    r"(?i)(?<=mr\. )\w+",
    r"(?<=a)a",
    r"(?<=a)(b*)*c",
    r"(?<=a)(b)?(?(1)c|d)",
    r"(?<=Mr\. )\g<1>(\w)",
    r"(?<=x)((a|aa)+)\1",
    // Forward literal and byte-set filters, which only reject seek candidates.
    r"#(\d)\1",
    r"(?:ab|cd)(\d)\1",
    r"\b(\w+) \1 \1\b",
    r#"r(#+)"(?s:.*?)"\1"#,
    r"((a|aa)+)\1",
    r"(?(DEFINE)(?<d>\d))\g<d>-\g<d>",
];

const TEXTS: [&str; 7] = [
    "Mr. Holmes and Dr. Watson; mr. x Dr.Who MR. Ab Dr. été",
    "foo.bar(1).baz_2(x) .qux( a.B( v.é(",
    "xab xabc é#1 €x ab aab abcc abdd a#1 #11 cd22 12-33 aaa\r\nfoo\n",
    "the the the cat cat sat on on on the mat",
    r###"let s = r#"x"#; r##"y"## r#"open"###,
    "xaaaaaaaaaaaaaaaaaaaaaaa!",
    "",
];

#[test]
fn seek_configurations_equal_the_oracle_under_every_limit() {
    for pattern in PATTERNS {
        for config in CONFIGS {
            for limit in [
                None,
                Some(0),
                Some(1),
                Some(2),
                Some(3),
                Some(5),
                Some(9),
                Some(40),
            ] {
                let regex = build(pattern, config, limit);
                let reference = build_oracle(pattern, config, limit);
                for text in TEXTS {
                    assert_eq!(
                        observe!(regex, text, RegexInput),
                        observe!(reference, text, OracleInput),
                        "{pattern:?} {config} {limit:?} {text:?}"
                    );
                }
            }
        }
    }
}

/// One searcher and one set of capture locations per configuration, across
/// hits, misses and limit errors, report what fresh searches report.
#[test]
fn retained_storage_follows_composed_seeks() {
    for pattern in PATTERNS {
        for config in CONFIGS {
            for limit in [None, Some(3)] {
                let regex = build(pattern, config, limit);
                let mut search = regex.searcher();
                let mut locations = regex.capture_locations();
                for text in TEXTS.iter().chain(TEXTS.iter().rev()) {
                    let context = format!("{pattern:?} {config} {limit:?} {text:?}");
                    let expected = regex.captures(text).map(|c| {
                        c.map(|c| {
                            c.iter()
                                .map(|m| m.map(|m| (m.start(), m.end())))
                                .collect::<Spans>()
                        })
                    });
                    let found = search.captures_read(&mut locations, text);
                    match &expected {
                        Ok(groups) => {
                            let read: Spans =
                                (0..locations.len()).map(|i| locations.get(i)).collect();
                            assert_eq!(
                                found.unwrap().map(|m| m.range()),
                                groups.as_ref().map(|g| g[0].map(|(s, e)| s..e).unwrap()),
                                "{context}"
                            );
                            let empty = vec![None; locations.len()];
                            assert_eq!(&read, groups.as_ref().unwrap_or(&empty), "{context}");
                        }
                        Err(_) => assert!(found.is_err(), "{context}"),
                    }
                    assert_eq!(
                        search.is_match(text).ok(),
                        expected.as_ref().ok().map(Option::is_some),
                        "{context}"
                    );
                }
            }
        }
    }
}

/// Plain seeking would attempt every capitalized letter here, each failing
/// once at the lookbehind, and past a million of them exceed the default
/// limit, as the exhaustive scan does. Under the default limit the pattern
/// is instead searched by its plan, whose candidates are where the
/// lookbehind's literals end: there are none, so the true answer is
/// returned. That was once a limit error; the default limit only bounded
/// incidental work. When the lookbehind's literals lead the seek there are no
/// candidates either. An explicit limit keeps seeking alone and its exact
/// error.
#[test]
fn lookbehind_literals_lead_seek_only_under_the_default_limit() {
    let pattern = r"(?<=Mr\. )[A-Z]";
    let text = "A".repeat(1_100_000);
    let find =
        |regex: &Regex, text: &str| format!("{:?}", regex.find(text).map(|m| m.map(|m| m.range())));
    let limit_error = "Err(RuntimeError(BacktrackLimitExceeded))";
    let default = build(pattern, "default", None);
    assert!(default.planned());
    assert_eq!(find(&default, &text), "Ok(None)");
    assert_eq!(find(&build(pattern, "seek", None), &text), "Ok(None)");
    assert_eq!(
        find(&build(pattern, "seek", Some(1_000_000)), &text),
        limit_error
    );
    assert!(build_oracle(pattern, "seek", None).find(&text).is_err());
    assert!(
        build_oracle(pattern, "seek", Some(1_000_000))
            .find(&text)
            .is_err()
    );
    let hit = format!("{text}Mr. B");
    let expected = format!("Ok(Some({}..{}))", hit.len() - 1, hit.len());
    assert_eq!(find(&build(pattern, "seek", None), &hit), expected);
}

/// A seek candidate whose attempt overflows the backtrack stack still reports
/// that error, whichever engine leads.
#[test]
fn seek_attempts_keep_stack_overflow() {
    let pattern = r"(?<=Mr\. )\w*\b!";
    let text = format!("Mr. {}!", "é".repeat(1_000_001));
    for config in CONFIGS {
        for limit in [None, Some(usize::MAX)] {
            let found = build(pattern, config, limit).find(&text);
            assert!(
                matches!(found, Err(Error::RuntimeError(RuntimeError::StackOverflow))),
                "{config} {limit:?} {found:?}"
            );
        }
    }
    assert!(build_oracle(pattern, "seek", None).find(&text).is_err());
}
