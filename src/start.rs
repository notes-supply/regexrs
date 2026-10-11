// Copyright 2026 Notes Supply AB. Distributed under the MIT license.
//! Start positions an unanchored VM search can skip without changing results
//! or errors.
//!
//! The unanchored preamble attempts a match at every position. When the
//! program begins with a bounded tree of branches in which every path fails at
//! a first-character test (a literal, class or delegated sub-expression at the
//! attempt position), an attempt at a position where none of those tests can
//! succeed fails along every path: it performs a fixed number of failures,
//! pushes only a bounded number of branches and cannot error. Such positions
//! are skipped with a literal search, and each one is charged that number of
//! backtracks, so matches and limit errors are those of the exhaustive scan.
//! When every test needs at least two bytes, a candidate's second byte must
//! also be one some test can begin with.
//!
//! A program that begins with a fixed-width lookbehind whose body is a set of
//! literals of one byte length fails inside the lookbehind, once, wherever
//! none of them ends at the attempt position. Candidates are then the ends of
//! their occurrences, which are usually far rarer than the first characters
//! the rest of the program tests.

use regex_automata::util::prefilter::Prefilter;
use regex_automata::{MatchKind, Span};
use regex_syntax::hir::Hir;
use regex_syntax::hir::literal::{ExtractKind, Extractor};

use crate::BytesMode;
use crate::vm::{CharClassMatcher, FoldRanges, Insn};

/// Instructions examined before giving up, which also bounds the branches a
/// skipped attempt can push.
const BUDGET: usize = 64;

/// Longest delegate literal prefix extracted for a shared needle; longer
/// prefixes add little selectivity but grow case-insensitive cross products.
const DELEGATE_LITERAL_LEN: usize = 4;

/// Most literals a leading lookbehind's body may expand to.
const BEHIND_LITERALS: usize = 64;

/// Candidate positions for unanchored attempts and the cost of the others.
#[derive(Debug)]
pub(crate) struct StartFilter {
    candidates: Candidates,
    /// Bytes a candidate's next byte must be one of, when known.
    seconds: Option<ByteSet>,
    /// Backtracks an attempt performs at any position that is not a candidate.
    cost: usize,
}

#[derive(Debug)]
enum Candidates {
    /// memchr or memmem through regex-automata.
    Literal(Prefilter),
    /// More first bytes than memchr handles, or no memchr support, as a bit
    /// set. This is regex-automata's own byte-set strategy, which it does not
    /// expose; `Prefilter::new` would build Teddy at many times the cost.
    Bytes(ByteSet),
    /// The ends of occurrences of a leading lookbehind's literals, all `len`
    /// bytes long, at which the next byte is in `firsts` when that is known.
    Behind {
        literals: Prefilter,
        len: usize,
        firsts: Option<ByteSet>,
    },
}

/// A set of bytes, one bit each.
#[derive(Clone, Copy, Debug, Default)]
struct ByteSet([u64; 4]);

impl ByteSet {
    fn insert(&mut self, byte: u8) {
        self.0[usize::from(byte >> 6)] |= 1 << (byte & 63);
    }

    fn contains(&self, byte: u8) -> bool {
        self.0[usize::from(byte >> 6)] & (1 << (byte & 63)) != 0
    }

    fn len(&self) -> u32 {
        self.0.iter().map(|word| word.count_ones()).sum()
    }
}

/// The literal every candidate must begin with, while all tests are exact
/// literals.
enum Common {
    /// No literal test seen yet.
    Unknown,
    Prefix(alloc::vec::Vec<u8>),
    /// A test without a literal prefix was seen.
    None,
}

impl Common {
    fn narrow(&mut self, literal: alloc::vec::Vec<u8>) {
        match self {
            Common::Unknown => *self = Common::Prefix(literal),
            Common::Prefix(prefix) => {
                let shared = prefix
                    .iter()
                    .zip(&literal)
                    .take_while(|(a, b)| a == b)
                    .count();
                prefix.truncate(shared);
            }
            Common::None => {}
        }
    }
}

/// Append the bytes every match of a literal test begins with; `Some(true)`
/// when they are the whole test, `None` for any other instruction.
fn exact_prefix(insn: &Insn, needle: &mut alloc::vec::Vec<u8>) -> Option<bool> {
    match insn {
        Insn::Lit(literal) => {
            needle.extend_from_slice(literal.as_bytes());
            Some(true)
        }
        Insn::LitCasei(literal) => {
            for folds in literal.chars() {
                let &[(lo, hi)] = folds.as_ref() else {
                    return Some(false);
                };
                if lo != hi {
                    return Some(false);
                }
                needle.extend_from_slice(lo.encode_utf8(&mut [0; 4]).as_bytes());
            }
            Some(true)
        }
        _ => None,
    }
}

impl StartFilter {
    /// Analyze the program whose first instruction after the unanchored
    /// preamble is `start`. `delegates` holds the `Hir` each delegate engine
    /// was built from, by instruction index. `None` when skipping cannot be
    /// shown to be exact, or when too many first bytes remain possible for
    /// skipping to pay off.
    pub(crate) fn new(
        body: &[Insn],
        start: usize,
        delegates: &[(usize, Hir)],
        bytes_mode: BytesMode,
    ) -> Option<Self> {
        let mut walk = Walk {
            budget: BUDGET,
            first_bytes: ByteSet::default(),
            seconds: Some(ByteSet::default()),
            common: Common::Unknown,
            delegates,
        };
        let start_path = Path {
            at_start: true,
            start_slots: 0,
        };
        let cost = walk.failures(body, start, start_path);
        let first_bytes = walk.first_bytes;
        // Whole-character stepping never visits a continuation byte, and
        // 0x80..=0xBF is exactly the third word of the set.
        let continuation = first_bytes.0[2] != 0;
        let firsts_usable =
            first_bytes.len() <= 128 && (!continuation || bytes_mode == BytesMode::Ascii);
        let shared_prefix = matches!(&walk.common, Common::Prefix(prefix) if prefix.len() >= 2);
        // A lookbehind's literals are preferred unless the walk found a
        // needle every candidate starts with. A walk through the lookbehind
        // costs one failure, as the lookbehind alone does.
        if !(cost.is_some() && firsts_usable && shared_prefix)
            && let Some((literals, len)) = behind_literals(body, start, delegates, bytes_mode)
            && let Some(literals) = Prefilter::new(MatchKind::LeftmostFirst, &literals)
        {
            let firsts = (cost == Some(1) && firsts_usable).then_some(first_bytes);
            return Some(StartFilter {
                candidates: Candidates::Behind {
                    literals,
                    len,
                    firsts,
                },
                seconds: None,
                cost: 1,
            });
        }
        let cost = cost?;
        if !firsts_usable {
            return None;
        }
        // A needle already tests the second byte; a set nearly every byte is
        // in rejects too little to pay for the check.
        let mut seconds = walk.seconds.filter(|set| set.len() <= 128);
        let literal = match walk.common {
            Common::Prefix(prefix) if prefix.len() >= 2 => {
                seconds = None;
                Prefilter::new(MatchKind::LeftmostFirst, &[prefix])
            }
            _ if (1..=3).contains(&first_bytes.len()) => {
                let mut needles = [[0u8; 1]; 3];
                let mut count = 0;
                for (word_index, &word) in first_bytes.0.iter().enumerate() {
                    let mut bits = word;
                    while bits != 0 {
                        needles[count] = [(word_index * 64) as u8 + bits.trailing_zeros() as u8];
                        count += 1;
                        bits &= bits - 1;
                    }
                }
                Prefilter::new(MatchKind::LeftmostFirst, &needles[..count])
            }
            _ => None,
        };
        let candidates = literal.map_or(Candidates::Bytes(first_bytes), Candidates::Literal);
        Some(StartFilter {
            candidates,
            seconds,
            cost,
        })
    }

    /// Backtracks an attempt performs at a position that is not a candidate.
    pub(crate) fn cost(&self) -> usize {
        self.cost
    }

    /// Whether candidates are where a leading lookbehind's literals end.
    pub(crate) fn follows_lookbehind(&self) -> bool {
        matches!(self.candidates, Candidates::Behind { .. })
    }

    /// The first candidate in `from..=end`.
    pub(crate) fn next(&self, haystack: &[u8], from: usize, end: usize) -> Option<usize> {
        self.find(haystack, from).filter(|&at| at <= end)
    }

    /// Whether `at` is a candidate, tested at that position alone; positions
    /// where it is not fail at the fixed cost.
    pub(crate) fn admits(&self, haystack: &[u8], at: usize) -> bool {
        let byte_in = |set: &ByteSet, at: usize| haystack.get(at).is_some_and(|&b| set.contains(b));
        let first = match &self.candidates {
            Candidates::Literal(prefilter) => prefilter
                .prefix(haystack, Span::from(at..haystack.len()))
                .is_some(),
            Candidates::Bytes(set) => byte_in(set, at),
            Candidates::Behind {
                literals,
                len,
                firsts,
            } => {
                at >= *len
                    && literals
                        .prefix(haystack, Span::from(at - len..at))
                        .is_some()
                    && firsts.as_ref().is_none_or(|firsts| byte_in(firsts, at))
            }
        };
        first
            && self
                .seconds
                .as_ref()
                .is_none_or(|seconds| byte_in(seconds, at + 1))
    }

    /// The first candidate at or after `start`.
    fn find(&self, haystack: &[u8], mut start: usize) -> Option<usize> {
        loop {
            let found = match &self.candidates {
                Candidates::Literal(prefilter) => prefilter
                    .find(haystack, Span::from(start..haystack.len()))
                    .map(|span| span.start),
                Candidates::Bytes(set) => haystack[start..]
                    .iter()
                    .position(|&byte| set.contains(byte))
                    .map(|offset| start + offset),
                Candidates::Behind {
                    literals,
                    len,
                    firsts,
                } => {
                    // An occurrence ending at `start` or later starts at most
                    // `len` bytes earlier.
                    let span = Span::from(start.saturating_sub(*len)..haystack.len());
                    let end = literals.find(haystack, span)?.start + len;
                    if end > haystack.len() {
                        return None;
                    }
                    match firsts {
                        Some(firsts) if !haystack.get(end).is_some_and(|&b| firsts.contains(b)) => {
                            start = end + 1;
                            continue;
                        }
                        _ => Some(end),
                    }
                }
            }?;
            match &self.seconds {
                Some(seconds)
                    if !haystack
                        .get(found + 1)
                        .is_some_and(|&b| seconds.contains(b)) =>
                {
                    start = found + 1;
                }
                _ => return Some(found),
            }
        }
    }

    /// The first candidate at or before `end` for an unanchored run at `from`
    /// in `haystack` (see [`skipped_cost`](Self::skipped_cost) for `text`),
    /// and the backtracks the exhaustive scan performs at the positions
    /// skipped to reach it, or at all remaining ones when there is none. A
    /// run that ends here only compares that count with the `room` left under
    /// the limit, so a byte count that fits stands in for the exact count.
    pub(crate) fn advance(
        &self,
        haystack: &[u8],
        text: Option<&str>,
        from: usize,
        end: usize,
        room: usize,
    ) -> (Option<usize>, usize) {
        let candidate = self.find(haystack, from).filter(|&at| at <= end);
        let stop = candidate.unwrap_or(end.saturating_add(1));
        let len = haystack.len();
        let bound = (stop.min(len) - from + usize::from(stop > len)).saturating_mul(self.cost);
        let cost = if candidate.is_none() && bound <= room {
            bound
        } else {
            self.skipped_cost(haystack, text, from, stop)
        };
        (candidate, cost)
    }

    /// Backtracks the exhaustive scan performs at the attempt positions in
    /// `from..stop`, where `stop` may be one past the end of `haystack`. With
    /// `text`, the scan steps whole characters of that valid UTF-8; without
    /// it, single bytes.
    fn skipped_cost(&self, haystack: &[u8], text: Option<&str>, from: usize, stop: usize) -> usize {
        let end = stop.min(haystack.len());
        let positions = match text {
            None => end - from,
            // A ranged search can end inside a character; count its
            // boundaries directly then.
            Some(text) => text.get(from..end).map_or_else(
                || {
                    haystack[from..end]
                        .iter()
                        .filter(|&&byte| (byte as i8) >= -0x40)
                        .count()
                },
                |part| part.chars().count(),
            ),
        } + usize::from(stop > haystack.len());
        positions.saturating_mul(self.cost)
    }
}

/// The literals a leading fixed-width lookbehind requires to end at the
/// attempt position, and their common byte length: the program saves the
/// attempt start in a slot, steps back `n` characters, tests literals that are
/// exactly the body's matches, each of `n` characters, and restores the slot.
/// Saves before it neither fail nor push, and every test fails at most once,
/// so an attempt where none of the literals ends fails exactly once.
fn behind_literals(
    body: &[Insn],
    start: usize,
    delegates: &[(usize, Hir)],
    bytes_mode: BytesMode,
) -> Option<(alloc::vec::Vec<alloc::vec::Vec<u8>>, usize)> {
    let mut pc = start;
    let (slot, chars) = loop {
        match (body.get(pc)?, body.get(pc + 1)?) {
            (Insn::Save(slot), Insn::GoBack(chars)) => break (*slot, *chars),
            (Insn::Save(_) | Insn::Save0(_) | Insn::SaveCaptureGroupStart(_), _) => pc += 1,
            _ => return None,
        }
    };
    let mut literals = alloc::vec![alloc::vec::Vec::new()];
    pc += 2;
    loop {
        let insn = body.get(pc)?;
        match insn {
            Insn::Restore(restored) if *restored == slot => break,
            // Fails at most once and consumes nothing; the literals then
            // include every text the body matches.
            Insn::Assertion(_) => {}
            Insn::Lit(literal) => {
                for text in &mut literals {
                    text.extend_from_slice(literal.as_bytes());
                }
            }
            Insn::LitCasei(literal) => {
                for folds in literal.chars() {
                    literals = extend(&literals, &chars_of(folds)?)?;
                }
            }
            Insn::CharClass(CharClassMatcher::Codepoint { ranges, .. }) => {
                literals = extend(&literals, &chars_of(ranges)?)?;
            }
            Insn::CharClass(CharClassMatcher::Byte { ranges, .. }) => {
                let mut bytes = alloc::vec::Vec::new();
                for &(lo, hi) in ranges.iter() {
                    if bytes.len() + usize::from(hi - lo) >= BEHIND_LITERALS {
                        return None;
                    }
                    bytes.extend((lo..=hi).map(|byte| alloc::vec![byte]));
                }
                literals = extend(&literals, &bytes)?;
            }
            Insn::Delegate(_) => {
                let (_, hir) = delegates.iter().find(|(at, _)| *at == pc)?;
                let matches = Extractor::new().kind(ExtractKind::Prefix).extract(hir);
                let texts = matches.literals().filter(|_| matches.is_exact())?;
                let texts: alloc::vec::Vec<_> =
                    texts.iter().map(|text| text.as_bytes().to_vec()).collect();
                literals = extend(&literals, &texts)?;
            }
            _ => return None,
        }
        pc += 1;
    }
    let len = literals.first()?.len();
    let width = |literal: &[u8]| match bytes_mode {
        BytesMode::Ascii => Some(literal.len()),
        _ => core::str::from_utf8(literal)
            .ok()
            .map(|text| text.chars().count()),
    };
    (len > 0
        && literals
            .iter()
            .all(|literal| literal.len() == len && width(literal) == Some(chars)))
    .then_some((literals, len))
}

/// The UTF-8 encodings of the characters in `ranges`, unless there are more
/// than `BEHIND_LITERALS`.
fn chars_of(ranges: &[(char, char)]) -> Option<alloc::vec::Vec<alloc::vec::Vec<u8>>> {
    let mut chars = alloc::vec::Vec::new();
    for &(lo, hi) in ranges {
        for c in lo..=hi {
            if chars.len() == BEHIND_LITERALS {
                return None;
            }
            chars.push(c.encode_utf8(&mut [0; 4]).as_bytes().to_vec());
        }
    }
    Some(chars)
}

/// Each of `literals` followed by each of `texts`, unless that makes more
/// than `BEHIND_LITERALS`.
fn extend(
    literals: &[alloc::vec::Vec<u8>],
    texts: &[alloc::vec::Vec<u8>],
) -> Option<alloc::vec::Vec<alloc::vec::Vec<u8>>> {
    if literals.len() * texts.len() > BEHIND_LITERALS {
        return None;
    }
    Some(
        literals
            .iter()
            .flat_map(|literal| {
                texts
                    .iter()
                    .map(move |text| [literal.as_slice(), text].concat())
            })
            .collect(),
    )
}

struct Walk<'a> {
    budget: usize,
    first_bytes: ByteSet,
    /// The second bytes of the tests' texts, until a test that can match a
    /// single byte or whose second byte is not tracked.
    seconds: Option<ByteSet>,
    common: Common,
    delegates: &'a [(usize, Hir)],
}

#[derive(Clone, Copy)]
struct Path {
    /// Whether the input position is still the attempt start.
    at_start: bool,
    /// Save slots below 64 currently holding the attempt start.
    start_slots: u64,
}

impl Path {
    fn save(&mut self, slot: usize, at_start: bool) {
        if let Some(bit) = u32::try_from(slot)
            .ok()
            .and_then(|slot| 1u64.checked_shl(slot))
        {
            self.start_slots &= !bit;
            if at_start {
                self.start_slots |= bit;
            }
        }
    }

    fn holds_start(&self, slot: usize) -> bool {
        u32::try_from(slot)
            .ok()
            .and_then(|slot| 1u64.checked_shl(slot))
            .is_some_and(|bit| self.start_slots & bit != 0)
    }
}

impl Walk<'_> {
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
                path.at_start = path.holds_start(*slot);
                self.failures(body, pc + 1, path)
            }
            Insn::BeginAtomic => self.failures(body, pc + 1, path),
            Insn::Jmp(target) => self.failures(body, *target, path),
            Insn::Split(first, second) => {
                let first = self.failures(body, *first, path)?;
                Some(first + self.failures(body, *second, path)?)
            }
            Insn::Fail => Some(1),
            Insn::Lit(_) | Insn::LitCasei(_) if path.at_start => {
                // A straight line of literal tests fails once wherever its
                // exact prefix (literal text, then case-insensitive
                // characters without case variants) does not occur.
                let mut needle = alloc::vec::Vec::new();
                let mut next = pc;
                while let Some(true) = body
                    .get(next)
                    .and_then(|insn| exact_prefix(insn, &mut needle))
                {
                    next += 1;
                }
                match (needle.as_slice(), insn) {
                    ([first, rest @ ..], _) => {
                        self.first_bytes.insert(*first);
                        self.insert_second(rest.first().copied());
                        self.common.narrow(needle);
                    }
                    // A case-insensitive literal starting with a cased
                    // character: its folds fix the first bytes.
                    ([], Insn::LitCasei(literal)) => {
                        let chars = literal.chars();
                        self.insert_chars(chars.first()?);
                        self.insert_seconds(&chars[0], chars.get(1));
                    }
                    ([], _) => return None,
                }
                Some(1)
            }
            Insn::CharClass(CharClassMatcher::Codepoint { ranges, .. }) if path.at_start => {
                self.insert_chars(ranges);
                self.seconds = None;
                Some(1)
            }
            Insn::CharClass(CharClassMatcher::Byte { ranges, .. }) if path.at_start => {
                self.common = Common::None;
                self.seconds = None;
                for &(lo, hi) in ranges.iter() {
                    for byte in lo..=hi {
                        self.first_bytes.insert(byte);
                    }
                }
                Some(1)
            }
            Insn::Delegate(_) if path.at_start => {
                // The engine is searched anchored here, so it fails unless
                // the text starts with one of its prefixes. An empty prefix
                // or an unbounded set rejects the filter.
                let (_, hir) = self.delegates.iter().find(|(at, _)| *at == pc)?;
                let mut extractor = Extractor::new();
                extractor.kind(ExtractKind::Prefix).limit_literal_len(2);
                let firsts = extractor.extract(hir);
                let firsts = firsts.literals()?;
                for literal in firsts {
                    let (&first, rest) = literal.as_bytes().split_first()?;
                    self.first_bytes.insert(first);
                    self.insert_second(rest.first().copied());
                }
                // Longer prefixes can only lengthen a needle every candidate
                // shares, so skip their case-fold cross products otherwise.
                let open = match &self.common {
                    Common::Unknown => true,
                    Common::Prefix(prefix) => prefix.len() >= 2,
                    Common::None => false,
                };
                let shared = firsts
                    .iter()
                    .all(|literal| literal.as_bytes()[..1] == firsts[0].as_bytes()[..1]);
                let longer = (open && shared).then(|| {
                    extractor
                        .limit_literal_len(DELEGATE_LITERAL_LEN)
                        .extract(hir)
                });
                let prefixes = longer
                    .as_ref()
                    .and_then(|seq| seq.literals())
                    .unwrap_or(firsts);
                for literal in prefixes {
                    self.common.narrow(literal.as_bytes().to_vec());
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
    fn insert_chars(&mut self, ranges: &[(char, char)]) {
        self.common = Common::None;
        insert_leads(&mut self.first_bytes, ranges);
    }

    /// Record a test's second byte; `None` when it can match one byte.
    fn insert_second(&mut self, byte: Option<u8>) {
        match (byte, &mut self.seconds) {
            (Some(byte), Some(seconds)) => seconds.insert(byte),
            _ => self.seconds = None,
        }
    }

    /// Record the second bytes of texts that begin with a character of the
    /// fold class `first`, followed by one in `next` when it is a single byte.
    fn insert_seconds(&mut self, first: &[(char, char)], next: Option<&FoldRanges>) {
        for &(lo, hi) in first {
            if lo <= '\x7F' {
                match (next, &mut self.seconds) {
                    (Some(next), Some(seconds)) => insert_leads(seconds, next),
                    _ => self.seconds = None,
                }
            }
            // Case-fold classes have a few characters each.
            for c in lo.max('\u{80}')..=hi {
                self.insert_second(Some(c.encode_utf8(&mut [0; 4]).as_bytes()[1]));
            }
        }
    }
}

/// Record the first UTF-8 bytes of the characters in `ranges`.
fn insert_leads(set: &mut ByteSet, ranges: &[(char, char)]) {
    let lead = |c: char| c.encode_utf8(&mut [0; 4]).as_bytes()[0];
    for &(lo, hi) in ranges {
        for byte in u32::from(lo)..=u32::from(hi).min(0x7F) {
            set.insert(byte as u8);
        }
        if hi >= '\u{80}' {
            for byte in lead(lo.max('\u{80}'))..=lead(hi) {
                set.insert(byte);
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
            r"(?:ab|cd)(?!x)",
            r"(ab|é)\1",
            r"(?i)(?:ka|sa)(?=a)",
            r"(?i)(?>ß|σ)\b",
            r"(?:\bab|k)(?!x)",
            r"(?:|ab)(?!x)",
            r"(?:a*)(?!x)b",
            r"(?>event|incident) \d+:",
            r"(?i)731(?!x)",
            r"(?i)#1k(?=a)",
            r"(?i)(?<=a)1-ſ\b",
        ];
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        let mut patterns: Vec<String> = fixed.iter().map(|p| String::from(*p)).collect();
        patterns.extend((0..2000).map(|_| generated_pattern(&mut rng, 0)));
        let texts = generated_texts(&mut rng);
        let (mut rich, mut accelerated, mut paired, mut checked_starts, mut errors) =
            (0, 0, 0, 0, 0);
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
                paired += usize::from(filter.seconds.is_some());
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
            rich >= 800 && accelerated >= 300 && paired >= 60,
            "{accelerated}/{rich} accelerated, {paired} checking second bytes"
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
            (r"(?:ab|cd)(?!x)", 1),
            (r"(?>event|incident) \d+:", 1),
            (r"(?i)#1k(?!x)", 1),
            (r"(?<=x)[ab]b(?!c)", 1),
            (r"(?<=é#)\d", 1),
        ];
        let text = "zz ab xab cd abab zz é#1 ab €x";
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
                // Range ends at a boundary, inside a character and at the
                // haystack end; the oracle has no start filter.
                // Ending on the euro sign's first continuation byte leaves the
                // next attempt position inside the character.
                let mid_char = text.find('€').unwrap() + 1;
                for end in [9, text.find('é').unwrap() + 1, mid_char, text.len()] {
                    let ranged = regex
                        .find_input(RegexInput::new(text).range(3..end))
                        .map(|m| m.map(|m| m.range()));
                    let expected = reference
                        .find_input(oracle::RegexInput::new(text).range(3..end))
                        .map(|m| m.map(|m| m.range()));
                    assert_eq!(
                        alloc::format!("{ranged:?}"),
                        alloc::format!("{expected:?}"),
                        "{pattern:?} limit={limit} end={end}"
                    );
                }
            }
        }
    }

    #[test]
    fn leading_lookbehind_literals_select_candidates() {
        // Candidates are the ends of the lookbehind's literals, then checked
        // against the first bytes after it where the rest can be walked. A
        // skipped position fails once inside the lookbehind.
        let cases = [
            (r"(?<=Mr\. |Dr\. )[A-Z]\w+", false),
            (r"(?<=\.)[a-z][a-z0-9]*(?=\()", true),
            (r"(?<=é)x", true),
            (r"(?i)(?<=mr\. )\w", true),
            (r"(?<=\bDr\. )\w", true),
            (r"(?<=ab)(\w)\1", true),
            (r"(?<=[.,(])[a-z]\w*", false),
            (r"(?<=[éè] )x", true),
        ];
        let text = "Mr. Holmes, Dr. Watson; mR. x foo.bar(1) é x éx è xa Dr.Who abcc abdd";
        for (pattern, checks_firsts) in cases {
            let firsts =
                start_filter(&Regex::new(pattern).unwrap()).and_then(|filter| {
                    match &filter.candidates {
                        Candidates::Behind { firsts, .. } => Some(firsts.is_some()),
                        _ => None,
                    }
                });
            assert_eq!(firsts, Some(checks_firsts), "{pattern:?}");
            for limit in [None, Some(0), Some(1), Some(2), Some(5), Some(20), Some(80)] {
                let (Some(regex), Some(reference)) = build(pattern, false, limit) else {
                    unreachable!("{pattern:?} compiles");
                };
                assert_eq!(
                    observe!(regex, text),
                    observe!(reference, text),
                    "{pattern:?} limit={limit:?}"
                );
                for start in (0..=text.len()).filter(|&at| text.is_char_boundary(at)) {
                    for end in [start, text.len() - 9, text.len()] {
                        let Some(end) = Some(end).filter(|&end| end >= start) else {
                            continue;
                        };
                        let found = regex
                            .find_input(RegexInput::new(text).range(start..end))
                            .map(|m| m.map(|m| m.range()));
                        let expected = reference
                            .find_input(oracle::RegexInput::new(text).range(start..end))
                            .map(|m| m.map(|m| m.range()));
                        assert_eq!(
                            alloc::format!("{found:?}"),
                            alloc::format!("{expected:?}"),
                            "{pattern:?} limit={limit:?} {start}..{end}"
                        );
                    }
                }
            }
        }
        // Literals of different byte lengths, or a body that is not a finite
        // set of literals, keep the ordinary walk.
        for pattern in [r"(?i)(?<=k)a", r"(?<=\w)a", r"(?<=a|bc)d", r"(?<=[eé])x"] {
            let behind = start_filter(&Regex::new(pattern).unwrap())
                .is_some_and(|filter| matches!(filter.candidates, Candidates::Behind { .. }));
            assert!(!behind, "{pattern:?}");
        }
    }

    #[test]
    fn caseless_needles_keep_every_case_variant() {
        // Only characters whose fold class is themselves join a needle; a
        // contiguous fold range (DŽ, Dž, dž) or a wider partner (KELVIN SIGN
        // for k) must stay a set of first bytes.
        let cases = [
            (r"(?i)1ǅ(?!x)", ["1ǆa", "1Ǆa", "1ǅa", "1dža"]),
            (r"(?i)#k(?!b)", ["#ka", "#Ka", "#\u{212A}a", "#kb"]),
            (r"(?i)7ſ1\b", ["7s1", "7S1", "7ſ1", "7ſ1x"]),
        ];
        for (pattern, texts) in cases {
            let (Some(regex), Some(reference)) = build(pattern, false, None) else {
                unreachable!("{pattern:?} compiles");
            };
            assert!(start_filter(&regex).is_some(), "{pattern:?}");
            for text in texts {
                let padded = alloc::format!("zz {text} {text}");
                assert_eq!(
                    observe!(regex, padded.as_str()),
                    observe!(reference, padded.as_str()),
                    "{pattern:?} {padded:?}"
                );
            }
        }
    }

    #[test]
    fn delegate_prefixes_keep_needles_and_case_variants() {
        // First and second bytes come from two-byte prefixes and a longer
        // shared needle from a second extraction; many case-insensitive
        // alternatives stay within the extractor's limits at two bytes.
        let words = "alpha|bravo|charlie|delta|echo|foxtrot|golf|hotel|india|juliet|kilo|lima|mike|november|oscar|papa";
        let many = alloc::format!("(?i)(?>{words})(?!s)");
        let cases = [
            (
                r"(?>eventual|evening)(?!s)",
                4,
                &["evening", "eventual", "even", "EVENING"][..],
            ),
            (
                r"(?i)(?>eventual|evening)(?!s)",
                1,
                &["EVENING", "eVeNtUaL", "evens", "even"],
            ),
            (
                r"(?i)(?>café|ſtrasse)(?! x)",
                0,
                &["CAFÉ", "Strasse", "ſTRASSE", "STRASSE x"],
            ),
            (&many, 0, &["KILO", "\u{212A}ilo", "Papa", "papas", "pap"]),
        ];
        for (pattern, needle_len, texts) in cases {
            let (Some(regex), Some(reference)) = build(pattern, false, None) else {
                unreachable!("{pattern:?} compiles");
            };
            let filter = start_filter(&regex).expect(pattern);
            let found = match &filter.candidates {
                Candidates::Literal(prefilter) => prefilter.max_needle_len(),
                Candidates::Bytes(_) => 0,
                Candidates::Behind { .. } => unreachable!("{pattern:?} has no lookbehind"),
            };
            assert_eq!(found, needle_len, "{pattern:?}");
            for text in texts {
                let padded = alloc::format!("zz {text} {text}");
                assert_eq!(
                    observe!(regex, padded.as_str()),
                    observe!(reference, padded.as_str()),
                    "{pattern:?} {padded:?}"
                );
            }
        }
    }

    #[test]
    fn second_bytes_keep_every_two_byte_start() {
        // A candidate's next byte must continue some test: the second byte of
        // a wide first character, or the first byte of the character after a
        // one-byte one. A test that can match one byte disables the check.
        let cases = [
            (r"(?i)ka(?!x)", Some(true), "ka KA \u{212A}a \u{212A}A kb"),
            (r"(?i)st(?!x)", Some(true), "st ST ſt ſT sx s"),
            (r"(?i)é(?!x)", Some(true), "é É e éx"),
            (r"(?i)σ\b", Some(true), "σ Σ ς s σa"),
            (r"(?>ab|cd)(?!z)", Some(true), "ab cd ac abz a"),
            (r"(?>a|cd)(?!z)", Some(false), "a cd ab c"),
            (r"(?:ab|[xy])(?!z)", Some(false), "ab x y a"),
            (r"(?i)k(?!x)", Some(false), "k K \u{212A} kx"),
            (r"ab(?!c)", Some(false), "ab abc a"),
        ];
        for (pattern, checks_second, text) in cases {
            for limit in [None, Some(0), Some(1), Some(2), Some(3), Some(5), Some(8)] {
                let (Some(regex), Some(reference)) = build(pattern, false, limit) else {
                    unreachable!("{pattern:?} compiles");
                };
                let seconds = start_filter(&regex).map(|filter| filter.seconds.is_some());
                assert_eq!(seconds, checks_second, "{pattern:?}");
                for start in 0..=text.len() {
                    for end in start..=text.len() {
                        let found = regex
                            .find_input(RegexInput::new(text).range(start..end))
                            .map(|m| m.map(|m| m.range()));
                        let expected = reference
                            .find_input(oracle::RegexInput::new(text).range(start..end))
                            .map(|m| m.map(|m| m.range()));
                        assert_eq!(
                            alloc::format!("{found:?}"),
                            alloc::format!("{expected:?}"),
                            "{pattern:?} limit={limit:?} {text:?} {start}..{end}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn interior_utf8_starts_try_every_visited_position() {
        // A start inside a character steps by the lengths its continuation
        // bytes imply (from 1 in "é€": 1, 3, 5), so those runs must not skip.
        for (pattern, text, range, limit) in [
            (r"(?>ab|cd)(?!z)", "é", 1..1, 0),
            (r"(?>ab|cd)(?!z)", "é€", 1..5, 2),
            (r"(?>event|incident) \d+:", "é", 1..1, 0),
            (r"(?>event|incident) \d+:", "é€", 1..5, 2),
        ] {
            let (Some(regex), Some(reference)) = build(pattern, false, Some(limit)) else {
                unreachable!("{pattern:?} compiles");
            };
            let found = regex.find_input(RegexInput::new(text).range(range.clone()));
            let expected = reference.find_input(oracle::RegexInput::new(text).range(range));
            assert!(expected.is_err(), "{pattern:?} {text:?}");
            assert_eq!(alloc::format!("{found:?}"), alloc::format!("{expected:?}"));
        }
        let patterns = [
            r"(?>ab|cd)(?!z)",
            r"(?>|ab)(?!z)",
            r"ab(?!c)",
            r"(?i)k(?!x)",
            r"(?:ab|é)(?!z)",
            r"\b€\d?(?!x)",
            r"(?<=é)𝄞(?!x)",
            r"#(\d)\1",
        ];
        let texts = ["é€𝄞 ab", "a€b𝄞cd é"];
        let modes = [
            BytesMode::Unicode,
            BytesMode::UnicodeBytes,
            BytesMode::Ascii,
        ];
        let spans = |r: Option<crate::Match<'_>>| r.map(|m| m.range());
        let groups = |c: Option<crate::Captures<'_, str>>| {
            c.map(|c| c.iter().map(|m| m.map(|m| m.range())).collect::<Vec<_>>())
        };
        let (mut builds, mut checks) = (0, 0);
        for pattern in patterns {
            for mode in modes {
                for limit in [0, 1, 2, 3, 4, 5, 8] {
                    let candidate = RegexBuilder::new(pattern)
                        .bytes_mode(mode)
                        .backtrack_limit(limit)
                        .build();
                    let reference = oracle::RegexBuilder::new(pattern)
                        .bytes_mode(match mode {
                            BytesMode::Unicode => oracle::BytesMode::Unicode,
                            BytesMode::UnicodeBytes => oracle::BytesMode::UnicodeBytes,
                            BytesMode::Ascii => oracle::BytesMode::Ascii,
                        })
                        .backtrack_limit(limit)
                        .build();
                    let (regex, reference) = match (candidate, reference) {
                        (Ok(regex), Ok(reference)) => (regex, reference),
                        (Err(_), Err(_)) => continue,
                        (candidate, reference) => panic!(
                            "{pattern:?} {mode:?} compile disagreement: {:?} vs {:?}",
                            candidate.err(),
                            reference.err()
                        ),
                    };
                    builds += 1;
                    for text in texts {
                        for start in 0..=text.len() {
                            for end in start..=text.len() {
                                let found = regex
                                    .find_input(RegexInput::new(text).range(start..end))
                                    .map(spans);
                                let expected = reference
                                    .find_input(oracle::RegexInput::new(text).range(start..end))
                                    .map(|m| m.map(|m| m.range()));
                                assert_eq!(
                                    alloc::format!("{found:?}"),
                                    alloc::format!("{expected:?}"),
                                    "{pattern:?} {mode:?} limit={limit} {text:?} {start}..{end}"
                                );
                                checks += 1;
                            }
                            let found = regex
                                .captures_input(RegexInput::new(text).from_pos(start))
                                .map(groups);
                            let expected = reference
                                .captures_input(oracle::RegexInput::new(text).from_pos(start))
                                .map(|c| {
                                    c.map(|c| {
                                        c.iter().map(|m| m.map(|m| m.range())).collect::<Vec<_>>()
                                    })
                                });
                            assert_eq!(
                                alloc::format!("{found:?}"),
                                alloc::format!("{expected:?}"),
                                "{pattern:?} {mode:?} limit={limit} {text:?} from {start}"
                            );
                            checks += 1;
                        }
                    }
                }
            }
        }
        assert!(
            builds >= 100 && checks >= 20_000,
            "{builds} builds, {checks} checks"
        );
    }

    #[test]
    fn lookbehind_literals_follow_every_bytes_mode() {
        // Byte classes in ASCII mode, and invalid UTF-8 around the literals,
        // which only ASCII mode skips over.
        let haystack: &[u8] = b"\xFF.ab,\xE2cd.\xFFe .f\xC3\xA9g";
        for pattern in [r"(?<=[.,])[a-z]+", r"(?<=\.)\w", r"(?<=\. )[a-z]"] {
            for (mode, reference_mode) in [
                (BytesMode::Unicode, oracle::BytesMode::Unicode),
                (BytesMode::UnicodeBytes, oracle::BytesMode::UnicodeBytes),
                (BytesMode::Ascii, oracle::BytesMode::Ascii),
            ] {
                let regex = RegexBuilder::new(pattern).bytes_mode(mode).build().unwrap();
                let behind = start_filter(&regex)
                    .is_some_and(|filter| matches!(filter.candidates, Candidates::Behind { .. }));
                assert!(behind, "{pattern:?} {mode:?}");
                let reference = oracle::RegexBuilder::new(pattern)
                    .bytes_mode(reference_mode)
                    .build()
                    .unwrap();
                let spans: Vec<_> = regex
                    .find_iter(haystack)
                    .map(|m| m.unwrap().range())
                    .collect();
                let expected: Vec<_> = reference
                    .find_iter(haystack)
                    .map(|m| m.unwrap().range())
                    .collect();
                assert_eq!(spans, expected, "{pattern:?} {mode:?}");
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
