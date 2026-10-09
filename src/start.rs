// Copyright 2026 Notes Supply AB. Distributed under the MIT license.
//! Start positions an unanchored VM search can skip without changing results
//! or errors.
//!
//! The unanchored preamble attempts a match at every position. When the
//! program begins with a bounded tree of branches in which every path fails at
//! a first-character test (a literal or class at the attempt position), an
//! attempt at a position where none of those tests can succeed fails along
//! every path: it performs a fixed number of failures, pushes only a bounded
//! number of branches and cannot error. Such positions are skipped with a
//! literal search, and each one is charged that number of backtracks, so
//! matches and limit errors are those of the exhaustive scan.

use alloc::boxed::Box;
use alloc::vec::Vec;
use regex_automata::util::prefilter::Prefilter;
use regex_automata::{MatchKind, Span};

use crate::BytesMode;
use crate::vm::Insn;

/// Instructions examined before giving up, which also bounds the branches a
/// skipped attempt can push.
const BUDGET: usize = 64;

/// Candidate positions for unanchored attempts and the cost of the others.
#[derive(Debug)]
pub(crate) struct StartFilter {
    candidates: Candidates,
    /// Backtracks an attempt performs at any position that is not a candidate.
    cost: usize,
}

#[derive(Debug)]
enum Candidates {
    /// memchr or memmem through regex-automata.
    Literal(Prefilter),
    /// More first bytes than memchr handles, or no memchr support. For these
    /// `Prefilter::new` would build Teddy at many times the construction
    /// cost; this is regex-automata's own byte-set strategy, which it does not
    /// expose.
    Bytes(Box<[bool; 256]>),
}

impl StartFilter {
    /// Analyze the program whose first instruction after the unanchored
    /// preamble is `start`. `None` when skipping cannot be shown to be exact,
    /// or when too many first bytes remain possible for skipping to pay off.
    pub(crate) fn new(body: &[Insn], start: usize) -> Option<Self> {
        let mut walk = Walk {
            budget: BUDGET,
            first_bytes: [false; 256],
            literals: Some(Vec::new()),
        };
        let path = Path {
            at_start: true,
            start_slots: Vec::new(),
        };
        let cost = walk.failures(body, start, path)?;
        let first_bytes: Vec<u8> = (0..=u8::MAX)
            .filter(|&byte| walk.first_bytes[usize::from(byte)])
            .collect();
        if first_bytes.len() > 128 {
            return None;
        }
        let common = walk
            .literals
            .as_deref()
            .and_then(|literals| literals.split_first())
            .map(|(first, rest)| {
                rest.iter().fold(first.as_slice(), |common, literal| {
                    let shared = common
                        .iter()
                        .zip(literal)
                        .take_while(|(a, b)| a == b)
                        .count();
                    &common[..shared]
                })
            })
            .unwrap_or_default();
        let literal = if common.len() >= 2 {
            Prefilter::new(MatchKind::LeftmostFirst, &[common])
        } else if !first_bytes.is_empty() && first_bytes.len() <= 3 {
            Prefilter::new(
                MatchKind::LeftmostFirst,
                &first_bytes.chunks(1).collect::<Vec<_>>(),
            )
        } else {
            None
        };
        let candidates = literal.map_or_else(
            || Candidates::Bytes(Box::new(walk.first_bytes)),
            Candidates::Literal,
        );
        Some(StartFilter { candidates, cost })
    }

    /// The first candidate at or after `start`.
    pub(crate) fn find(&self, haystack: &[u8], start: usize) -> Option<usize> {
        match &self.candidates {
            Candidates::Literal(prefilter) => prefilter
                .find(haystack, Span::from(start..haystack.len()))
                .map(|span| span.start),
            Candidates::Bytes(set) => haystack[start..]
                .iter()
                .position(|&byte| set[usize::from(byte)])
                .map(|offset| start + offset),
        }
    }

    /// Backtracks the exhaustive scan performs at the attempt positions in
    /// `from..stop`, where `stop` may be one past the end of `haystack`.
    /// Outside ASCII mode the scan steps whole characters of valid UTF-8.
    pub(crate) fn skipped_cost(
        &self,
        haystack: &[u8],
        from: usize,
        stop: usize,
        bytes_mode: BytesMode,
    ) -> usize {
        let end = stop.min(haystack.len());
        let positions = match bytes_mode {
            BytesMode::Ascii => end - from,
            BytesMode::Unicode | BytesMode::UnicodeBytes => haystack[from..end]
                .iter()
                .filter(|&&byte| (byte as i8) >= -0x40)
                .count(),
        } + usize::from(stop > haystack.len());
        positions.saturating_mul(self.cost)
    }
}

struct Walk {
    budget: usize,
    first_bytes: [bool; 256],
    /// Every leaf's literal while all leaves are exact literals.
    literals: Option<Vec<Vec<u8>>>,
}

#[derive(Clone)]
struct Path {
    /// Whether the input position is still the attempt start.
    at_start: bool,
    /// Save slots currently holding the attempt start.
    start_slots: Vec<usize>,
}

impl Path {
    fn save(&mut self, slot: usize, at_start: bool) {
        self.start_slots.retain(|&saved| saved != slot);
        if at_start {
            self.start_slots.push(slot);
        }
    }
}

impl Walk {
    /// Failures performed from `pc` at a non-candidate position, collecting
    /// the first-character tests that define the candidates. `None` when the
    /// count could depend on the text or the path could succeed.
    fn failures(&mut self, body: &[Insn], pc: usize, mut path: Path) -> Option<usize> {
        self.budget = self.budget.checked_sub(1)?;
        let insn = body.get(pc)?;
        match insn {
            Insn::Save(slot) => {
                path.save(*slot, path.at_start);
                self.failures(body, pc + 1, path)
            }
            Insn::Save0(slot) => {
                path.save(*slot, false);
                self.failures(body, pc + 1, path)
            }
            Insn::SaveCaptureGroupStart(group) => {
                path.save(group * 2, false);
                self.failures(body, pc + 1, path)
            }
            Insn::Restore(slot) => {
                path.at_start = path.start_slots.contains(slot);
                self.failures(body, pc + 1, path)
            }
            Insn::BeginAtomic => self.failures(body, pc + 1, path),
            Insn::Jmp(target) => self.failures(body, *target, path),
            Insn::Split(first, second) => {
                let first = self.failures(body, *first, path.clone())?;
                Some(first + self.failures(body, *second, path)?)
            }
            Insn::Fail => Some(1),
            Insn::Lit(literal) if path.at_start => {
                // Adjacent literals form one needle; a straight line of them
                // fails once wherever the whole needle does not occur.
                let mut needle = literal.as_bytes().to_vec();
                let mut next = pc + 1;
                while let Some(Insn::Lit(more)) = body.get(next) {
                    needle.extend_from_slice(more.as_bytes());
                    next += 1;
                }
                self.first_bytes[usize::from(*needle.first()?)] = true;
                if let Some(literals) = &mut self.literals {
                    literals.push(needle);
                }
                Some(1)
            }
            Insn::LitCasei(literal) if path.at_start => {
                self.mark_chars(literal.first_char()?);
                Some(1)
            }
            Insn::CharClass(class) if path.at_start => {
                match class {
                    crate::vm::CharClassMatcher::Codepoint { ranges, .. } => {
                        self.mark_chars(ranges);
                    }
                    crate::vm::CharClassMatcher::Byte { ranges, .. } => {
                        self.literals = None;
                        for &(lo, hi) in ranges.iter() {
                            for byte in lo..=hi {
                                self.first_bytes[usize::from(byte)] = true;
                            }
                        }
                    }
                }
                Some(1)
            }
            // These can fail but never push or error. Failing here costs one
            // failure, so the rest must cost exactly one as well.
            Insn::Assertion(_)
            | Insn::BackrefExistsCondition(_)
            | Insn::GoBack(_)
            | Insn::Lit(_)
            | Insn::LitCasei(_)
            | Insn::CharClass(_)
            | Insn::Any
            | Insn::AnyNoNL
            | Insn::AnyNoCRLF
            | Insn::Backref { .. }
            | Insn::Delegate(_) => {
                path.at_start &=
                    matches!(insn, Insn::Assertion(_) | Insn::BackrefExistsCondition(_));
                (self.failures(body, pc + 1, path)? == 1).then_some(1)
            }
            // Anything else could succeed, push unboundedly, cut branches or
            // return early, so its cost is not known.
            _ => None,
        }
    }

    /// Record the first UTF-8 bytes of the characters in `ranges`.
    fn mark_chars(&mut self, ranges: &[(char, char)]) {
        self.literals = None;
        let lead = |c: char| c.encode_utf8(&mut [0; 4]).as_bytes()[0];
        for &(lo, hi) in ranges {
            for byte in u32::from(lo)..=u32::from(hi).min(0x7F) {
                self.first_bytes[byte as usize] = true;
            }
            if hi >= '\u{80}' {
                for byte in lead(lo.max('\u{80}'))..=lead(hi) {
                    self.first_bytes[usize::from(byte)] = true;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Regex, RegexBuilder, RegexImpl, RegexInput};
    use alloc::string::String;
    use alloc::vec;

    /// Every match with its capture spans, then `find`'s span; an error as its
    /// variant name. Expands for both this crate and the oracle.
    macro_rules! observe {
        ($regex:expr, $text:expr) => {
            (|| -> core::result::Result<Vec<Vec<Option<(usize, usize)>>>, String> {
                let error = |e| alloc::format!("{e:?}");
                let mut all = Vec::new();
                for captures in $regex.captures_iter($text) {
                    let captures = captures.map_err(error)?;
                    all.push(
                        captures
                            .iter()
                            .map(|m| m.map(|m| (m.start(), m.end())))
                            .collect(),
                    );
                }
                let found = $regex.find($text).map_err(error)?;
                all.push(vec![found.map(|m| (m.start(), m.end()))]);
                Ok(all)
            })()
        };
    }

    fn build(
        pattern: &str,
        casei: bool,
        limit: Option<usize>,
    ) -> (Option<Regex>, Option<oracle::Regex>) {
        let mut candidate = RegexBuilder::new(pattern);
        let mut reference = oracle::RegexBuilder::new(pattern);
        candidate.case_insensitive(casei);
        reference.case_insensitive(casei);
        if let Some(limit) = limit {
            candidate.backtrack_limit(limit);
            reference.backtrack_limit(limit);
        }
        let (candidate, reference) = (candidate.build(), reference.build());
        assert_eq!(
            candidate.is_ok(),
            reference.is_ok(),
            "{pattern:?} compile disagreement: {:?} vs {:?}",
            candidate.as_ref().err(),
            reference.as_ref().err()
        );
        (candidate.ok(), reference.ok())
    }

    fn start_filter(regex: &Regex) -> Option<&StartFilter> {
        match &regex.inner {
            RegexImpl::Fancy { prog, .. } => prog.start_filter(),
            RegexImpl::Wrap { .. } => None,
        }
    }

    /// Deterministic xorshift, so failures reproduce from the printed pattern.
    struct Rng(u64);

    impl Rng {
        fn pick<'a>(&mut self, items: &[&'a str]) -> &'a str {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            items[(self.0 % items.len() as u64) as usize]
        }
    }

    fn generated_pattern(rng: &mut Rng, depth: usize) -> String {
        const ATOMS: &[&str] = &[
            "a", "a", "b", "ab", "ab", "é", "k", "s", "σ", "ß", "1-", "[ab]", r"\d", r"\w", ".",
            "()", r"\b", r"\B", "^", "$", r"\A", r"\z", r"\K", r"\G", "(?=a)", "(?!b)", "(?<=a)",
            "(?<!b)", r"\1", r"\2",
        ];
        const WRAPS: &[&str] = &[
            "({})",
            "(?:{})",
            "(?i:{})",
            "(?>{})",
            "{}*",
            "{}+",
            "{}?",
            "(?:{}){2}",
            "{}*?",
            "(?:{})++",
            "(?:{}|a)",
            "(?:{}|)",
            "(?:|{})",
            "(?={})",
            "(?!{})",
            "({})\\1",
            "(?(1){}|b)",
            "(a)?(?(1){}|b)",
        ];
        let units = 1 + (rng.pick(&["0", "1", "2"]).as_bytes()[0] - b'0') as usize;
        let mut pattern = String::new();
        for _ in 0..units {
            if depth < 2 && rng.pick(&["atom", "wrap"]) == "wrap" {
                let inner = generated_pattern(rng, depth + 1);
                pattern.push_str(&rng.pick(WRAPS).replace("{}", &inner));
            } else {
                pattern.push_str(rng.pick(ATOMS));
            }
        }
        pattern
    }

    fn generated_texts(rng: &mut Rng) -> Vec<String> {
        // Weighted towards the generator's literals, plus case-fold partners
        // with different UTF-8 widths: k/K/KELVIN SIGN, s/S/LONG S, sigma
        // forms and sharp s.
        const PIECES: &[&str] = &[
            "a", "a", "a", "b", "b", "ab", "A", "é", "É", "k", "K", "\u{212A}", "s", "S", "ſ", "σ",
            "ς", "Σ", "ß", "ẞ", "1", " ", "\n",
        ];
        let mut texts = vec![
            String::new(),
            "ab".into(),
            "aab".into(),
            "abab".into(),
            "kK\u{212A}".into(),
            "sSſ".into(),
            "σςΣ".into(),
            "ßẞ".into(),
        ];
        for length in 1..=8 {
            for _ in 0..10 {
                texts.push((0..length).map(|_| rng.pick(PIECES)).collect());
            }
        }
        texts
    }

    #[test]
    fn skipped_starts_keep_oracle_matches_and_limit_errors() {
        let fixed = [
            r"((a|ab)+)\1",
            r"(?i)(ſ)\1",
            r"(?i)(k)\1",
            r"(?i)ß(?=a)",
            r"(?i)(σ)+\1",
            r"()\1a",
            r"(a|)\1b",
            r"(?<=a)b",
            r"(?=ab)a",
            r"a(?=b)",
            r"(a)?(?(1)b|c)",
            r"a\Kb",
            r"\Ga(?!b)",
            r"(?:a|b|)\1",
            r"(?m)^a(?!b)",
            r"(?>a|ab)b",
            r"(a)\1(?=b)",
            r"(?i)é(?<!a)",
            r"(?x) a (?! b )",
            r"(?i)(?:ab|ka)(?!b)",
            r"(?:a|b|é|k|s)(?=a)\1?",
            r"(?i)(?>a|s|é)\b",
            r"(?i)(?:σ|k)(a)\1",
            r"(?=((a|aa)+)\2$)z",
            r"\b(?:a|b)(?!c)",
            r"(?<=a)(?:b|é)\1?",
        ];
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        let mut patterns: Vec<String> = fixed.iter().map(|p| String::from(*p)).collect();
        patterns.extend((0..2000).map(|_| generated_pattern(&mut rng, 0)));
        let texts = generated_texts(&mut rng);
        let (mut rich, mut accelerated, mut checked_starts, mut errors) = (0, 0, 0, 0);
        for (index, pattern) in patterns.iter().enumerate() {
            // Case-insensitive builds for every third pattern; inline flags
            // cover the rest of the generated mix.
            let casei = index % 3 == 2;
            // Tiny limits cross the threshold within a few start positions,
            // so skipped attempts must be charged exactly what they cost.
            for limit in [None, Some(2), Some(9)] {
                let (Some(regex), Some(reference)) = build(pattern, casei, limit) else {
                    continue;
                };
                for text in &texts {
                    let expected = observe!(reference, text);
                    errors += usize::from(expected.is_err());
                    assert_eq!(
                        observe!(regex, text),
                        expected,
                        "{pattern:?} casei={casei} limit={limit:?} {text:?}"
                    );
                }
                if limit.is_some() {
                    continue;
                }
                rich += usize::from(matches!(regex.inner, RegexImpl::Fancy { .. }));
                let Some(filter) = start_filter(&regex) else {
                    continue;
                };
                accelerated += 1;
                // Judge the filter directly: an anchored search never consults
                // it, so a skipped viable start cannot hide behind the matcher.
                for text in &texts {
                    for (start, _) in text.char_indices().chain([(text.len(), ' ')]) {
                        let input = RegexInput::new(text.as_str())
                            .from_pos(start)
                            .anchored(true);
                        if regex.find_input(input).unwrap().is_some() {
                            checked_starts += 1;
                            assert_eq!(
                                filter.find(text.as_bytes(), start),
                                Some(start),
                                "{pattern:?} casei={casei} skipped viable start {start} in {text:?}"
                            );
                        }
                    }
                }
            }
        }
        // A mostly-fallback corpus would not exercise the skipping path.
        assert!(
            rich >= 800 && accelerated >= 300,
            "{accelerated}/{rich} accelerated"
        );
        assert!(
            checked_starts >= 3_000,
            "{checked_starts} viable starts checked"
        );
        assert!(errors >= 10_000, "{errors} limit errors compared");
    }

    #[test]
    fn skipped_starts_keep_default_limit_errors() {
        // Per-position accounting on a long subject, and an expensive
        // lookahead executed before the literal a seek approximation sees.
        let cases = [
            (r"#(\d)\1", "a".repeat(1_100_000)),
            (r"(?=((a|aa)+)\2$)z", alloc::format!("{}!", "a".repeat(40))),
        ];
        for (pattern, text) in cases {
            for limit in [None, Some(1_000_000)] {
                let (Some(regex), Some(reference)) = build(pattern, false, limit) else {
                    unreachable!("{pattern:?} compiles");
                };
                let expected = reference.find(&text).map(|m| m.map(|m| m.range()));
                assert!(expected.is_err(), "{pattern:?} limit={limit:?}");
                assert_eq!(
                    alloc::format!("{:?}", regex.find(&text).map(|m| m.map(|m| m.range()))),
                    alloc::format!("{expected:?}"),
                    "{pattern:?} limit={limit:?}"
                );
            }
        }
    }

    #[test]
    fn skipped_starts_reach_the_limit_at_the_oracle_threshold() {
        // Filters with one and two failures per skipped position, an
        // assertion and a lookbehind before the literal, and positions
        // charged up to the end of a ranged subject.
        let patterns = [
            (r"#(\d)\1", 1),
            (r"\bab(?!c)", 1),
            (r"(?<=x)ab(?!c)", 1),
            (r"((a|ab)+)\1", 2),
            (r"(?:absent|missing)(\d+): key-\1", 2),
        ];
        let text = "zz ab xab cd abab zz é#1 ab";
        for (pattern, cost) in patterns {
            assert_eq!(
                start_filter(&Regex::new(pattern).unwrap()).map(|filter| filter.cost),
                Some(cost),
                "{pattern:?}"
            );
            for limit in 0..80 {
                let (Some(regex), Some(reference)) = build(pattern, false, Some(limit)) else {
                    unreachable!("{pattern:?} compiles");
                };
                assert_eq!(
                    observe!(regex, text),
                    observe!(reference, text),
                    "{pattern:?} limit={limit}"
                );
                let ranged = |end| RegexInput::new(text).range(3..end);
                for end in [9, text.len()] {
                    assert_eq!(
                        alloc::format!(
                            "{:?}",
                            regex.find_input(ranged(end)).map(|m| m.map(|m| m.range()))
                        ),
                        alloc::format!(
                            "{:?}",
                            RegexBuilder::new(pattern)
                                .backtrack_limit(limit)
                                .build()
                                .unwrap()
                                .find_input(ranged(end))
                                .map(|m| m.map(|m| m.range()))
                        ),
                    );
                }
            }
        }
    }

    #[test]
    fn skipping_follows_whole_character_stepping_on_bytes() {
        // Whole-character stepping from an invalid lead byte never visits
        // offset 1 or 4, so an `A` there is not a match outside ASCII mode.
        let haystack: &[u8] = b"\xE2Ac\xF0A";
        for (mode, expected) in [
            (BytesMode::Unicode, vec![]),
            (BytesMode::UnicodeBytes, vec![]),
            (BytesMode::Ascii, vec![1..2, 4..5]),
        ] {
            let regex = RegexBuilder::new(r"A(?!b)")
                .bytes_mode(mode)
                .build()
                .unwrap();
            assert!(start_filter(&regex).is_some());
            let spans: Vec<_> = regex
                .find_iter(haystack)
                .map(|m| m.unwrap().range())
                .collect();
            assert_eq!(spans, expected, "{mode:?}");
        }
    }
}
