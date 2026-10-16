//! Construction keeps 0.1.5's size limits. A pattern that a plan
//! could search without the meta engine is still accepted or rejected
//! exactly where that engine's NFA size limit decides, with the same error,
//! by `Regex::new` and `RegularRegex::new` alike.
//!
//! The boundaries were measured on 0.1.5 with the locked regex-automata
//! 0.4.18: the first count named for each family is the largest that builds.

use regexrs::{Regex, RegexBuilder, RegularRegex};

const DEFAULT_LIMIT: &str = "CompileError(InnerError(BuildError { kind: NFA(BuildError { \
                             kind: ExceededSizeLimit { limit: 10485760 } }) }))";

fn regex(pattern: &str) -> Result<usize, String> {
    Regex::new(pattern)
        .map(|regex| regex.captures_len())
        .map_err(|error| format!("{error:?}"))
}

fn regular(pattern: &str) -> Result<usize, String> {
    RegularRegex::new(pattern)
        .map(|regex| regex.captures_len())
        .map_err(|error| format!("{error:?}"))
}

fn repeated(prefix: &str, unit: &str, copies: usize, suffix: &str) -> String {
    format!("{prefix}{}{suffix}", unit.repeat(copies))
}

/// Capture groups, assertions and literal bytes each cost the engine's NFA
/// space; none of them may be left out when deciding whether the engine must
/// be built to report its limit. One test builds them in turn to bound peak
/// memory.
#[test]
fn both_constructors_keep_the_default_nfa_limit() {
    // (prefix, unit, suffix, largest accepted copies, groups per copy)
    let families = [
        ("", "()", "a", 109_224, 1),
        ("", "(a)", "", 109_224, 1),
        ("", "(?m:^)", "a", 327_672, 0),
        ("a", "(?m:$)", "", 327_672, 0),
        ("", "a", "", 327_673, 0),
    ];
    for (prefix, unit, suffix, accepted, groups) in families {
        let fits = repeated(prefix, unit, accepted, suffix);
        let captures = Ok(1 + groups * accepted);
        assert_eq!(regex(&fits), captures, "Regex {unit} x{accepted}");
        assert_eq!(regular(&fits), captures, "RegularRegex {unit} x{accepted}");
        let exceeds = repeated(prefix, unit, accepted + 1, suffix);
        let rejected = Err(DEFAULT_LIMIT.to_string());
        assert_eq!(regex(&exceeds), rejected, "Regex {unit} x{}", accepted + 1);
        assert_eq!(
            regular(&exceeds),
            rejected,
            "RegularRegex {unit} x{}",
            accepted + 1
        );
    }

    // Word boundaries keep `Regex` on the backtracking VM; `RegularRegex`
    // builds the engine.
    assert_eq!(regular(&repeated("", r"\b", 327_672, "a")), Ok(1));
    assert_eq!(
        regular(&repeated("", r"\b", 327_673, "a")),
        Err(DEFAULT_LIMIT.to_string())
    );

    // Far from the boundary on either side, as independent acceptance probed.
    assert_eq!(regex(&repeated("", "()", 100_000, "a")), Ok(100_001));
    assert_eq!(regular(&repeated("", "()", 100_000, "a")), Ok(100_001));
    assert_eq!(
        regex(&repeated("", "()", 200_000, "a")),
        Err(DEFAULT_LIMIT.to_string())
    );
    assert_eq!(
        regular(&repeated("", "()", 200_000, "a")),
        Err(DEFAULT_LIMIT.to_string())
    );
}

/// An explicit size limit always builds the engine, so its errors are the
/// engine's. A pure literal needs no NFA and passes any NFA limit, and the
/// lazy DFA's limit never fails construction.
#[test]
fn both_constructors_keep_explicit_size_limits() {
    let limited = |pattern: &str, limit: usize| {
        let mut builder = RegexBuilder::new(pattern);
        builder.delegate_size_limit(limit);
        let regex = builder
            .build()
            .map(|regex| regex.captures_len())
            .map_err(|error| format!("{error:?}"));
        let regular = builder
            .build_regular()
            .map(|regex| regex.captures_len())
            .map_err(|error| format!("{error:?}"));
        (regex, regular)
    };
    let exceeded = |limit: usize| {
        Err(format!(
            "CompileError(InnerError(BuildError {{ kind: NFA(BuildError {{ kind: \
             ExceededSizeLimit {{ limit: {limit} }} }}) }}))"
        ))
    };
    for pattern in [r"\w+", r#""[^"]*""#] {
        assert_eq!(
            limited(pattern, 1000),
            (exceeded(1000), exceeded(1000)),
            "{pattern}"
        );
        assert_eq!(limited(pattern, 1 << 20), (Ok(1), Ok(1)), "{pattern}");
    }
    assert_eq!(
        limited(r"\w+", 10_000),
        (exceeded(10_000), exceeded(10_000))
    );
    assert_eq!(limited(r#""[^"]*""#, 10_000), (Ok(1), Ok(1)));
    assert_eq!(limited(r"\b\w+\b", 1000), (Ok(1), exceeded(1000)));
    assert_eq!(limited("Sherlock Holmes", 1), (Ok(1), Ok(1)));

    let mut builder = RegexBuilder::new(r"\w+");
    builder.delegate_dfa_size_limit(1);
    assert_eq!(
        builder.build().map(|regex| regex.captures_len()).ok(),
        Some(1)
    );
    assert_eq!(
        builder
            .build_regular()
            .map(|regex| regex.captures_len())
            .ok(),
        Some(1)
    );
}

/// The backtracking VM's delegated engines keep the default limit too: one
/// that could exceed it is built at construction and reports the engine's
/// error there, while one that certainly fits is built when first searched.
#[test]
fn vm_delegates_keep_the_default_nfa_limit() {
    assert_eq!(regex(r"(a)\1\w{400}"), Err(DEFAULT_LIMIT.to_string()));
    let fits = Regex::new(r"(a)\1([A-Z]\w+)").unwrap();
    let text = format!("x aaZ{}!", "é".repeat(30));
    let captures = fits.captures(&text).unwrap().expect("matches");
    assert_eq!(captures.get(2).map(|m| m.range()), Some(4..65));
    assert!(!fits.is_match("aa é").unwrap());
}
