use regexrs::{Captures, Error, NoExpand, RegexBuilder, RuntimeError};
use std::borrow::Cow;

mod common;

#[test]
fn replacer_string() {
    let regex = common::regex(
        r"\b([sS])uc(?:cs|s?)e(ed(?:ed|ing|s?)|ss(?:es|ful(?:ly)?|i(?:ons?|ve(?:ly)?)|ors?)?)\b",
    );

    // Replacer impl for &str
    let result = regex.replace("a sucessful b", "${1}ucce$2");
    assert_eq!(result, "a successful b");

    // Replacer impl for &String
    let repl_string = "${1}ucce$2".to_string();
    let result = regex.replace("a Suceeded b", &repl_string);
    assert_eq!(result, "a Succeeded b");

    // Replacer impl for String
    let result = regex.replace("a sucessor b", repl_string);
    assert_eq!(result, "a successor b");
}

#[test]
fn replacer_cow() {
    let regex = common::regex(r"\b([oO])mmi(?=t)t?(t(?:ed|ing)|s)\b");

    // Replacer impl for &Cow<str>
    let result = regex.replace("a ommiting b", &Cow::from("${1}mit$2"));
    assert_eq!(result, "a omitting b");

    // Replacer for Cow<str>::Borrowed
    let result = regex.replace("a ommited b", Cow::Borrowed("${1}mit$2"));
    assert_eq!(result, "a omitted b");

    // Replacer for Cow<str>::Owned
    let result = regex.replace("a Ommits b", Cow::Owned("${1}mit$2".to_string()));
    assert_eq!(result, "a Omits b");
}

#[test]
fn replacer_noexpand() {
    let regex = common::regex(r"\b([aA])n+ull(ar|ments?|s?)\b");

    // Replacer impl for NoExpand
    let result = regex.replace("a anullment b", NoExpand("${1}nnul$2"));
    assert_eq!(result, "a ${1}nnul$2 b");
}

#[test]
fn replacer_callback() {
    let regex = common::regex(r"\b([aA])p(?:p[or]|ro)x\.?(?=[ \)\n])");

    // Replacer impl for FnMut(&Captures)
    let result = regex.replace("a Aprox b", |cap: &Captures<'_, str>| {
        format!("{}pprox.", cap.get(1).unwrap().as_str())
    });
    assert_eq!(result, "a Approx. b");
}

/// `replace()` does only one replacement
#[test]
fn replace_one() {
    let regex = common::regex("bla");
    assert_eq!(regex.replace("blabla", "foo"), "foobla");
}

/// `replace_all()` replaces all non-overlapping matches
#[test]
fn replace_all() {
    let regex = common::regex("aa");
    assert_eq!(regex.replace_all("aaaa aaa aa a", "xx"), "xxxx xxa xx a");
}

/// `replacen()` replaces predefined number of times
#[test]
fn replacen() {
    let regex = common::regex("bla");
    assert_eq!(regex.replacen("blablabla", 2, "foo"), "foofoobla");
}

/// The fallible conveniences report the first matching error instead of
/// panicking, whatever the replacer.
#[test]
fn try_replace_reports_limit_errors() {
    let regex = RegexBuilder::new(r"(a+)+\1$")
        .backtrack_limit(100)
        .build()
        .unwrap();
    let text = "aaaaaaaaaaaaaaaaaaaa!";
    for result in [
        regex.try_replace(text, "x"),
        regex.try_replace_all(text, "x"),
        regex.try_replace_all(text, |caps: &Captures| caps[0].to_owned()),
    ] {
        assert!(matches!(
            result,
            Err(Error::RuntimeError(RuntimeError::BacktrackLimitExceeded))
        ));
    }
}

/// Successful results equal the panicking methods, including borrowing the
/// input when nothing matched.
#[test]
fn try_replace_equals_replace_on_success() {
    let regex = common::regex(r"(?<word>\w+)\s+\k<word>");
    let text = "the the cat cat sat";
    assert_eq!(
        regex.try_replace(text, "$word").unwrap(),
        regex.replace(text, "$word")
    );
    assert_eq!(regex.try_replace_all(text, "$word").unwrap(), "the cat sat");
    assert!(matches!(
        regex.try_replace_all("no repeats", "x").unwrap(),
        Cow::Borrowed("no repeats")
    ));
}

/// `Captures` defaults to `str`, so closures need no type arguments.
#[test]
fn replacement_closures_infer_str_captures() {
    let regex = common::regex(r"(\d+)");
    let doubled = regex.replace_all("1 22", |caps: &Captures| {
        (caps[1].parse::<u32>().unwrap() * 2).to_string()
    });
    assert_eq!(doubled, "2 44");
}
