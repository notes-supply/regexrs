//! Engines with working memory (the meta engine, the backtracking VM and a
//! route's automata) check it out of their pools for each search. Results
//! must not depend on which search holds which memory: nested iterations over
//! one regex, iterations interleaved with single searches, and iterations on
//! several threads at once.

use regexrs::{Regex, RegularRegex};

/// Patterns on the meta engine, on the VM, and on the VM with a word
/// boundary route, none of which takes a plan.
const PATTERNS: &[&str] = &[r"(a|ab)(c|bcd)(d*)", r"(\w)\1", r"\b(\w+)s\b", r"[a-z]+ing"];

const TEXT: &str = "abcd acdd abbcd aab xx yyz cats dogs a sing ring abcing ooh";

fn spans(regex: &Regex, text: &str) -> Vec<(usize, usize)> {
    regex
        .find_iter(text)
        .map(|m| m.map(|m| (m.start(), m.end())).unwrap())
        .collect()
}

#[test]
fn nested_and_interleaved_iterations_agree() {
    for pattern in PATTERNS {
        let regex = Regex::new(pattern).unwrap();
        assert!(!regex.planned(), "{pattern}");
        let expected = spans(&regex, TEXT);
        let mut outer = Vec::new();
        for m in regex.find_iter(TEXT) {
            let m = m.unwrap();
            outer.push((m.start(), m.end()));
            // Another iteration and single searches while the outer one
            // holds its memory.
            assert_eq!(spans(&regex, TEXT), expected, "{pattern}");
            assert_eq!(
                regex.find(TEXT).unwrap().map(|m| (m.start(), m.end())),
                expected.first().copied()
            );
            let captures: Vec<_> = regex
                .captures_iter(&TEXT[m.start()..])
                .map(|c| c.unwrap().get(0).map(|m| m.range()))
                .collect();
            assert_eq!(captures.len(), spans(&regex, &TEXT[m.start()..]).len());
        }
        assert_eq!(outer, expected, "{pattern}");
    }
    let regular = RegularRegex::new(PATTERNS[0]).unwrap();
    assert!(!regular.planned());
    let expected: Vec<_> = regular.find_iter(TEXT).map(|m| m.range()).collect();
    for m in regular.find_iter(TEXT) {
        let inner: Vec<_> = regular.find_iter(TEXT).map(|m| m.range()).collect();
        assert_eq!(inner, expected);
        assert!(regular.captures_iter(&TEXT[m.start()..]).count() > 0);
    }
}

#[cfg(feature = "std")]
#[test]
fn concurrent_iterations_agree() {
    use std::sync::Arc;
    for pattern in PATTERNS {
        let regex = Arc::new(Regex::new(pattern).unwrap());
        let regular = Arc::new(RegularRegex::new(r"(a|ab)(c|bcd)(d*)").unwrap());
        let text: String = TEXT.repeat(50);
        let expected = spans(&regex, &text);
        let expected_regular: Vec<_> = regular.find_iter(&text).map(|m| m.range()).collect();
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let (regex, regular, text) =
                    (Arc::clone(&regex), Arc::clone(&regular), text.clone());
                std::thread::spawn(move || {
                    let mut results = Vec::new();
                    for _ in 0..20 {
                        results.push((
                            spans(&regex, &text),
                            regular
                                .find_iter(&text)
                                .map(|m| m.range())
                                .collect::<Vec<_>>(),
                        ));
                    }
                    results
                })
            })
            .collect();
        for thread in threads {
            for (found, found_regular) in thread.join().unwrap() {
                assert_eq!(found, expected, "{pattern}");
                assert_eq!(found_regular, expected_regular);
            }
        }
    }
}
