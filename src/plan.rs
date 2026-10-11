// Copyright 2026 Notes Supply AB. Distributed under the MIT license.
//! Linear plans: forward search for regular patterns whose matches follow one
//! deterministic path.
//!
//! A plan is a sequence of literals and single-character runs, with capture
//! boundaries anywhere and look-around assertions only at its ends, such that
//! at most one way of matching exists from any start position: a run of
//! variable length is never followed by a character it could also consume,
//! and assertions after a run that could stop early fail wherever it would.
//! The leftmost-first match is then the first candidate position where the
//! sequence matches anchored, and greedy and lazy priorities coincide with
//! maximal or minimal runs. No reverse search, automaton state or cache is
//! needed. Patterns outside this shape keep their regex-automata engine.
//!
//! Work stays linear in the haystack. An attempt that fails after an
//! unbounded first run skips past that run, and a bounded first run is tried
//! once per maximal run of its class, from the starts that can reach its end.
//! No later run that the attempt can fail after holds a character that can
//! start a match, so only the bounded steps, which span at most
//! [`RESCAN_BOUND`] bytes, can hold the start of another attempt that scans
//! the same run again.
//!
//! Candidates come from the first literal, a literal at a fixed offset, a
//! regex-automata prefilter, memchr over at most three bytes, a word-at-a-time
//! test of byte ranges or a class scan, in that order of preference.

use alloc::boxed::Box;
use alloc::vec::Vec;
use regex_automata::util::captures::{Captures as RaCaptures, GroupInfo};
use regex_automata::util::lazy::Lazy;
use regex_automata::util::look::{Look, LookMatcher, LookSet};
use regex_automata::util::prefilter::Prefilter;
use regex_automata::util::primitives::NonMaxUsize;
use regex_automata::{MatchKind, PatternID, Span};
use regex_syntax::hir::{self, Hir, HirKind};

use crate::vm::{char_at, range_contains};

/// A regular pattern searched as one deterministic sequence of steps.
#[derive(Clone, Debug)]
pub(crate) struct LinearPlan {
    steps: Box<[Step]>,
    /// Assertions tested where a candidate starts.
    leading: Box<[Look]>,
    /// Assertions tested where the last step ends.
    trailing: Box<[Look]>,
    candidates: Candidates,
    /// The longest literal after the first step, which every match contains:
    /// a search where it does not occur ends at once.
    required: Option<Box<memchr::memmem::Finder<'static>>>,
    /// A leading `\A`: only the search's start can begin a match.
    start_only: bool,
    /// The first consuming step is an unbounded run: an attempt that fails
    /// after it fails from every later start inside the same run.
    skip_first_run: bool,
    /// The index of the first consuming step.
    first: usize,
    /// The first step is a bounded run that nothing after it can continue:
    /// within one maximal run of its class, only starts whose run reaches the
    /// end of it can match, and all of them continue alike from there.
    window: bool,
    /// How many steps decide whether a match exists. Every step after them
    /// can match nothing and no assertion follows, so a membership test stops
    /// there, and without trailing assertions its last run only needs its
    /// minimum.
    decisive: usize,
    look_matcher: LookMatcher,
    /// The capture groups of the pattern, with their names.
    group_info: GroupInfo,
    /// An upper bound on the size of the pattern's Thompson NFA; see
    /// [`nfa_bound`].
    nfa_units: usize,
}

#[derive(Clone, Debug)]
enum Step {
    Literal(Box<[u8]>),
    Run(Run),
    /// Record the current position in this capture slot.
    Save(usize),
}

#[derive(Clone, Debug)]
struct Run {
    class: Class,
    min: usize,
    /// `usize::MAX` when unbounded.
    max: usize,
    /// Consume `min` characters only: a lazy run that nothing after it can
    /// fail. Every other run consumes as much as it can, which is also what a
    /// lazy run does when the next character must end it.
    shortest: bool,
    /// For a class of every scalar value but at most three ASCII bytes, those
    /// bytes: in valid UTF-8 the run ends at the next of them.
    stops: Option<Stops>,
}

#[derive(Clone, Copy, Debug)]
enum Stops {
    None,
    One(u8),
    Two(u8, u8),
    Three(u8, u8, u8),
}

/// The largest [`LinearPlan::nfa_units`] for which the meta engine's default
/// NFA size limit certainly accepts the pattern, so its engine need not be
/// built to find out. Against that limit, a unit of any component costs at
/// most about 66 bytes (astral classes), so a pattern within the budget needs
/// at most about 540 KiB of the 10 MiB allowed. Repeated components first
/// exceed the limit at about 160,000 units (astral classes), 200,000
/// (optional characters) and 327,000 (captures, assertions, literal bytes).
pub(crate) const NFA_UNIT_BUDGET: usize = 8192;

/// The most bytes a plan's bounded steps may span when an attempt can fail,
/// so each position is scanned by at most this many attempts.
const RESCAN_BOUND: usize = 64;

/// The characters a step can consume, one at a time.
#[derive(Clone, Debug)]
enum Class {
    /// Unicode scalar values, as UTF-8. Invalid UTF-8 is never a member.
    Chars(Box<CharSet>),
    /// Single bytes.
    Bytes([u64; 4]),
}

#[derive(Clone, Debug)]
struct CharSet {
    ascii: u128,
    /// U+0080 to U+07FF, which encode in two bytes, one bit each.
    two: [u64; 30],
    /// Sorted inclusive ranges from U+0800.
    wide: Box<[(char, char)]>,
}

#[derive(Clone, Debug)]
enum Candidates {
    /// The plan starts with this literal.
    Literal(Box<memchr::memmem::Finder<'static>>),
    /// Every match starts with one of a prefilter's literals.
    Prefilter(Prefilter),
    /// Every match starts with one of these bytes.
    Bytes(Stops),
    /// Every match starts with one of these bytes, which the class confirms.
    Ranges(ByteRanges, Class),
    /// Every match holds this literal this many bytes after its start.
    Offset(Box<memchr::memmem::Finder<'static>>, usize),
    /// The plan starts with a run of this class.
    Class(Class),
}

/// Bytes in at most two ASCII ranges, and any non-ASCII byte when `high` is
/// set, tested eight at a time within a 64-bit word.
#[derive(Clone, Copy, Debug)]
struct ByteRanges {
    /// Inclusive ranges within ASCII; unused ones are empty.
    ranges: [(u8, u8); 2],
    high: bool,
    /// The same bytes, one bit each, for testing them one at a time.
    bytes: [u64; 4],
}

impl ByteRanges {
    /// The ranges holding every byte of `bytes`, if there are few enough.
    fn of(bytes: &[u64; 4]) -> Option<ByteRanges> {
        let mut ranges = [(1, 0); 2];
        let mut count = 0;
        let mut b = 0;
        while b < 0x80 {
            if !has_byte(bytes, b) {
                b += 1;
                continue;
            }
            let lo = b;
            while b < 0x80 && has_byte(bytes, b) {
                b += 1;
            }
            *ranges.get_mut(count)? = (lo, b - 1);
            count += 1;
        }
        let high = bytes[2] | bytes[3] != 0;
        let mut all = *bytes;
        if high {
            all[2] = u64::MAX;
            all[3] = u64::MAX;
        }
        (count > 0).then_some(ByteRanges {
            ranges,
            high,
            bytes: all,
        })
    }

    /// A word whose high bit is set in each byte of `word` in the set. Each
    /// lane computes on its low seven bits, so nothing carries between them.
    #[inline]
    fn mask(&self, word: u64) -> u64 {
        const LANES: u64 = 0x0101_0101_0101_0101;
        let low = word & (LANES * 0x7F);
        let ascii = !word & (LANES * 0x80);
        let mut found = if self.high { word & (LANES * 0x80) } else { 0 };
        for (lo, hi) in self.ranges {
            // `128 + hi - b` and `b + 128 - lo` reach 128 when `lo <= b <= hi`.
            let below = (LANES * (128 + u64::from(hi))).wrapping_sub(low);
            let above = low.wrapping_add(LANES * (128 - u64::from(lo)));
            found |= below & above & ascii;
        }
        found & (LANES * 0x80)
    }

    #[inline]
    fn contains(&self, b: u8) -> bool {
        has_byte(&self.bytes, b)
    }

    /// The first position in `from..end` holding a byte in the set.
    #[inline]
    fn find(&self, haystack: &[u8], from: usize, end: usize) -> Option<usize> {
        // Dense classes usually have a member among the first bytes.
        let head = (from + 4).min(end);
        if let Some(offset) = haystack[from..head].iter().position(|&b| self.contains(b)) {
            return Some(from + offset);
        }
        let mut at = head;
        while let Some(word) = haystack[at..end].first_chunk::<8>() {
            let mask = self.mask(u64::from_le_bytes(*word));
            if mask != 0 {
                return Some(at + (mask.trailing_zeros() / 8) as usize);
            }
            at += 8;
        }
        haystack[at..end]
            .iter()
            .position(|&b| self.contains(b))
            .map(|offset| at + offset)
    }
}

/// `[0-9A-Za-z_]`.
const ASCII_WORD: u128 =
    (0x3FF << b'0') | (0x3FF_FFFF << b'A') | (0x3FF_FFFF << b'a') | (1 << b'_');

/// Whether each byte is an ASCII word character.
static WORD_BYTE: [bool; 256] = {
    let mut table = [false; 256];
    let mut b = 0;
    while b < 128 {
        table[b] = (ASCII_WORD >> b) & 1 == 1;
        b += 1;
    }
    table
};

fn has_byte(set: &[u64; 4], b: u8) -> bool {
    set[usize::from(b >> 6)] & (1 << (b & 63)) != 0
}

fn insert_byte(set: &mut [u64; 4], b: u8) {
    set[usize::from(b >> 6)] |= 1 << (b & 63);
}

/// Bit `b` of `bits`, for `b` below 128, read from one 64-bit half: a
/// variable 128-bit shift costs several instructions per character.
#[inline]
fn bit128(bits: u128, b: u8) -> bool {
    let half = if b < 64 {
        bits as u64
    } else {
        (bits >> 64) as u64
    };
    (half >> (b & 63)) & 1 == 1
}

/// The three- or four-byte UTF-8 encoding `bytes` starts with, and its
/// length, checked as `str::from_utf8` would: no overlong forms, surrogates
/// or values past U+10FFFF.
#[inline]
fn decode_wide(bytes: &[u8]) -> Option<(char, usize)> {
    let continuation = |b: u8| b & 0xC0 == 0x80;
    match *bytes {
        [lead @ 0xE0..=0xEF, b1, b2, ..] => {
            let second = match lead {
                0xE0 => (0xA0..=0xBF).contains(&b1),
                0xED => (0x80..=0x9F).contains(&b1),
                _ => continuation(b1),
            };
            if !second || !continuation(b2) {
                return None;
            }
            let value =
                (u32::from(lead & 0x0F) << 12) | (u32::from(b1 & 0x3F) << 6) | u32::from(b2 & 0x3F);
            char::from_u32(value).map(|c| (c, 3))
        }
        [lead @ 0xF0..=0xF4, b1, b2, b3, ..] => {
            let second = match lead {
                0xF0 => (0x90..=0xBF).contains(&b1),
                0xF4 => (0x80..=0x8F).contains(&b1),
                _ => continuation(b1),
            };
            if !second || !continuation(b2) || !continuation(b3) {
                return None;
            }
            let value = (u32::from(lead & 0x07) << 18)
                | (u32::from(b1 & 0x3F) << 12)
                | (u32::from(b2 & 0x3F) << 6)
                | u32::from(b3 & 0x3F);
            char::from_u32(value).map(|c| (c, 4))
        }
        _ => None,
    }
}

/// Whether `haystack` has no UTF-8 continuation byte at `at`.
fn char_boundary(haystack: &[u8], at: usize) -> bool {
    haystack.get(at).is_none_or(|&b| (b as i8) >= -0x40)
}

impl CharSet {
    fn new(ranges: impl IntoIterator<Item = (char, char)>) -> CharSet {
        let mut set = CharSet {
            ascii: 0,
            two: [0; 30],
            wide: Box::new([]),
        };
        let mut wide = Vec::new();
        for (start, end) in ranges {
            let (lo, hi) = (u32::from(start), u32::from(end));
            for c in lo..=hi.min(0x7F) {
                set.ascii |= 1 << c;
            }
            for c in lo.max(0x80)..=hi.min(0x7FF) {
                let index = (c - 0x80) as usize;
                set.two[index >> 6] |= 1 << (index & 63);
            }
            if hi >= 0x800 {
                wide.push((start.max('\u{800}'), end));
            }
        }
        set.wide = wide.into_boxed_slice();
        set
    }

    fn has_non_ascii(&self) -> bool {
        self.two.iter().any(|&bits| bits != 0) || !self.wide.is_empty()
    }

    /// The length of the member starting with the non-ASCII byte at `at`,
    /// which must end by `end`.
    #[inline]
    fn non_ascii_len(&self, haystack: &[u8], at: usize, end: usize) -> Option<usize> {
        let lead = haystack[at];
        if lead < 0xE0 {
            // A two-byte encoding, or a byte no encoding starts with.
            let next = *haystack[..end].get(at + 1)?;
            if lead < 0xC2 || next & 0xC0 != 0x80 {
                return None;
            }
            let index = ((usize::from(lead & 0x1F) << 6) | usize::from(next & 0x3F)) - 0x80;
            return ((self.two[index >> 6] >> (index & 63)) & 1 == 1).then_some(2);
        }
        if self.wide.is_empty() {
            return None;
        }
        let (c, len) = decode_wide(&haystack[at..end])?;
        range_contains(&self.wide, c).then_some(len)
    }

    fn is_subset(&self, other: &CharSet) -> bool {
        self.ascii & !other.ascii == 0
            && self.two.iter().zip(&other.two).all(|(a, b)| a & !b == 0)
            && self.wide.iter().all(|&(lo, hi)| {
                let at = other.wide.partition_point(|&(_, end)| end < lo);
                other
                    .wide
                    .get(at)
                    .is_some_and(|&(start, end)| start <= lo && hi <= end)
            })
    }
}

/// An upper bound on the UTF-8 sequences encoding `start..=end`: at most
/// `2n - 1` within each run of `n`-byte encodings.
fn sequence_bound(start: char, end: char) -> usize {
    let (start, end) = (u32::from(start), u32::from(end));
    [
        (0, 0x7F, 1),
        (0x80, 0x7FF, 3),
        (0x800, 0xD7FF, 5),
        (0xE000, 0xFFFF, 5),
        (0x1_0000, 0x10_FFFF, 7),
    ]
    .iter()
    .filter(|&&(lo, hi, _)| start <= hi && lo <= end)
    .map(|&(_, _, units)| units)
    .sum()
}

/// The units of a class: an upper bound on its UTF-8 sequences, or its byte
/// ranges.
fn class_units(class: &hir::Class) -> usize {
    match class {
        hir::Class::Unicode(class) => class
            .iter()
            .map(|range| sequence_bound(range.start(), range.end()))
            .fold(0, usize::saturating_add),
        hir::Class::Bytes(class) => class.ranges().len(),
    }
}

/// An upper bound on the size of the Thompson NFA regex-automata compiles from
/// `hir`, counting every node: a unit per literal byte, empty expression,
/// assertion and concatenation, a class's [`class_units`], two units per
/// capture group, and each copy of a repetition's expression plus one. No
/// node is left out, so a pattern whose engine the default size limit rejects
/// always exceeds [`NFA_UNIT_BUDGET`].
fn nfa_bound(hir: &Hir) -> usize {
    let own = match hir.kind() {
        HirKind::Empty | HirKind::Look(_) | HirKind::Concat(_) | HirKind::Alternation(_) => 1,
        HirKind::Literal(hir::Literal(bytes)) => bytes.len(),
        HirKind::Class(class) => class_units(class),
        HirKind::Capture(_) => 2,
        HirKind::Repetition(repetition) => {
            let copies = repetition.max.unwrap_or(repetition.min).max(1) as usize;
            return nfa_bound(&repetition.sub)
                .saturating_add(1)
                .saturating_mul(copies);
        }
    };
    hir.kind()
        .subs()
        .iter()
        .map(nfa_bound)
        .fold(own, usize::saturating_add)
}

impl Class {
    fn from_hir(class: &hir::Class) -> Class {
        match class {
            hir::Class::Unicode(class) => Class::Chars(Box::new(CharSet::new(
                class.iter().map(|range| (range.start(), range.end())),
            ))),
            hir::Class::Bytes(class) => {
                let mut set = [0u64; 4];
                for range in class.iter() {
                    for b in range.start()..=range.end() {
                        insert_byte(&mut set, b);
                    }
                }
                Class::Bytes(set)
            }
        }
    }

    /// The class of the one character, or byte, a literal holds.
    fn from_literal(bytes: &[u8]) -> Option<Class> {
        match char_at(bytes, 0) {
            Some((c, len)) if len == bytes.len() => {
                Some(Class::Chars(Box::new(CharSet::new([(c, c)]))))
            }
            _ => match *bytes {
                [b] => {
                    let mut set = [0u64; 4];
                    insert_byte(&mut set, b);
                    Some(Class::Bytes(set))
                }
                _ => None,
            },
        }
    }

    /// The length of the member at `at`, which must end by `end`.
    #[inline]
    fn match_len(&self, haystack: &[u8], at: usize, end: usize) -> Option<usize> {
        if at >= end {
            return None;
        }
        let b = haystack[at];
        match self {
            Class::Chars(set) if b < 0x80 => bit128(set.ascii, b).then_some(1),
            Class::Chars(set) => set.non_ascii_len(haystack, at, end),
            Class::Bytes(set) => has_byte(set, b).then_some(1),
        }
    }

    fn has_ascii(&self, b: u8) -> bool {
        match self {
            Class::Chars(set) => bit128(set.ascii, b),
            Class::Bytes(set) => has_byte(set, b),
        }
    }

    /// The most bytes one member takes.
    fn max_len(&self) -> usize {
        match self {
            Class::Chars(set) => match set.wide.last() {
                Some(&(_, end)) => end.len_utf8(),
                None if set.two.iter().any(|&bits| bits != 0) => 2,
                None => 1,
            },
            Class::Bytes(_) => 1,
        }
    }

    /// The bytes a class excludes when it is every scalar value but at most
    /// three ASCII ones.
    fn stops(&self) -> Option<Stops> {
        let Class::Chars(set) = self else {
            return None;
        };
        if set.ascii.count_zeros() > 3
            || set.two.iter().any(|&bits| bits != u64::MAX)
            || *set.wide != [('\u{800}', char::MAX)]
        {
            return None;
        }
        Stops::of(&mut (0..0x80u8).filter(|&b| (set.ascii >> b) & 1 == 0))
    }

    /// The bytes members can start with.
    fn first_bytes(&self) -> [u64; 4] {
        match self {
            Class::Bytes(set) => *set,
            Class::Chars(set) => {
                let mut bytes = [set.ascii as u64, (set.ascii >> 64) as u64, 0, 0];
                for index in 0..1920 {
                    if (set.two[index >> 6] >> (index & 63)) & 1 == 1 {
                        insert_byte(&mut bytes, 0xC0 | ((0x80 + index) >> 6) as u8);
                    }
                }
                let lead = |c: char| c.encode_utf8(&mut [0; 4]).as_bytes()[0];
                for &(lo, hi) in set.wide.iter() {
                    for b in lead(lo)..=lead(hi) {
                        insert_byte(&mut bytes, b);
                    }
                }
                bytes
            }
        }
    }

    /// Whether some character could be a member of both classes.
    fn overlaps(&self, other: &Class) -> bool {
        match (self, other) {
            (Class::Chars(a), Class::Chars(b)) => {
                a.ascii & b.ascii != 0
                    || a.two.iter().zip(&b.two).any(|(x, y)| x & y != 0)
                    || a.wide.iter().any(|&(lo, hi)| {
                        b.wide.iter().any(|&(start, end)| lo <= end && start <= hi)
                    })
            }
            (Class::Bytes(a), Class::Bytes(b)) => a.iter().zip(b).any(|(a, b)| a & b != 0),
            (Class::Chars(chars), Class::Bytes(bytes))
            | (Class::Bytes(bytes), Class::Chars(chars)) => {
                (u128::from(bytes[0]) | (u128::from(bytes[1]) << 64)) & chars.ascii != 0
                    || (bytes[2] | bytes[3] != 0 && chars.has_non_ascii())
            }
        }
    }

    /// Whether every member is a word character for word boundaries of this
    /// kind.
    fn is_word(&self, unicode: bool) -> bool {
        match self {
            Class::Bytes(set) => {
                set[2] | set[3] == 0
                    && (u128::from(set[0]) | (u128::from(set[1]) << 64)) & !ASCII_WORD == 0
            }
            Class::Chars(set) => {
                set.ascii & !ASCII_WORD == 0
                    && (!set.has_non_ascii()
                        || unicode
                            && Lazy::get(&UNICODE_WORD)
                                .as_ref()
                                .is_some_and(|word| set.is_subset(word)))
            }
        }
    }
}

/// Unicode's `\w`, which word boundary assertions also use, parsed once.
static UNICODE_WORD: Lazy<Option<CharSet>> = Lazy::new(unicode_word);

fn unicode_word() -> Option<CharSet> {
    let hir = regex_syntax::ParserBuilder::new()
        .build()
        .parse(r"\w")
        .ok()?;
    match hir.kind() {
        HirKind::Class(hir::Class::Unicode(class)) => Some(CharSet::new(
            class.iter().map(|range| (range.start(), range.end())),
        )),
        _ => None,
    }
}

impl Stops {
    fn of(bytes: &mut impl Iterator<Item = u8>) -> Option<Stops> {
        Some(
            match (bytes.next(), bytes.next(), bytes.next(), bytes.next()) {
                (None, ..) => Stops::None,
                (Some(a), None, ..) => Stops::One(a),
                (Some(a), Some(b), None, _) => Stops::Two(a, b),
                (Some(a), Some(b), Some(c), None) => Stops::Three(a, b, c),
                _ => return None,
            },
        )
    }

    #[inline]
    fn find(self, haystack: &[u8]) -> Option<usize> {
        match self {
            Stops::None => None,
            Stops::One(a) => memchr::memchr(a, haystack),
            Stops::Two(a, b) => memchr::memchr2(a, b, haystack),
            Stops::Three(a, b, c) => memchr::memchr3(a, b, c, haystack),
        }
    }
}

/// A flattened HIR element before admission.
enum Item {
    Look(Look),
    Literal(Vec<u8>),
    Run(Run, bool),
    Save(usize),
}

impl Step {
    /// The characters the step's first consumed character can be.
    fn first(&self) -> Option<Class> {
        match self {
            Step::Literal(bytes) => {
                let len = char_at(bytes, 0).map_or(1, |(_, len)| len);
                Class::from_literal(&bytes[..len])
            }
            Step::Run(run) => Some(run.class.clone()),
            Step::Save(_) => None,
        }
    }

    fn min_len(&self) -> usize {
        match self {
            Step::Literal(_) => 1,
            Step::Run(run) => run.min,
            Step::Save(_) => 0,
        }
    }

    /// The most bytes the step consumes, when bounded.
    fn max_len(&self) -> Option<usize> {
        match self {
            Step::Literal(bytes) => Some(bytes.len()),
            Step::Run(run) if run.max == usize::MAX => None,
            Step::Run(run) => Some(run.max.saturating_mul(run.class.max_len())),
            Step::Save(_) => Some(0),
        }
    }
}

fn look(look: hir::Look) -> Look {
    match look {
        hir::Look::Start => Look::Start,
        hir::Look::End => Look::End,
        hir::Look::StartLF => Look::StartLF,
        hir::Look::EndLF => Look::EndLF,
        hir::Look::StartCRLF => Look::StartCRLF,
        hir::Look::EndCRLF => Look::EndCRLF,
        hir::Look::WordAscii => Look::WordAscii,
        hir::Look::WordAsciiNegate => Look::WordAsciiNegate,
        hir::Look::WordUnicode => Look::WordUnicode,
        hir::Look::WordUnicodeNegate => Look::WordUnicodeNegate,
        hir::Look::WordStartAscii => Look::WordStartAscii,
        hir::Look::WordEndAscii => Look::WordEndAscii,
        hir::Look::WordStartUnicode => Look::WordStartUnicode,
        hir::Look::WordEndUnicode => Look::WordEndUnicode,
        hir::Look::WordStartHalfAscii => Look::WordStartHalfAscii,
        hir::Look::WordEndHalfAscii => Look::WordEndHalfAscii,
        hir::Look::WordStartHalfUnicode => Look::WordStartHalfUnicode,
        hir::Look::WordEndHalfUnicode => Look::WordEndHalfUnicode,
    }
}

/// Whether `look` fails wherever `run` could stop early: before another of
/// its characters, after at least `run.min` of them.
fn ends_run(look: Look, run: &Run) -> bool {
    match look {
        Look::End => true,
        Look::EndLF => !run.class.has_ascii(b'\n'),
        Look::EndCRLF => !run.class.has_ascii(b'\n') && !run.class.has_ascii(b'\r'),
        Look::WordAscii
        | Look::WordStartAscii
        | Look::WordEndAscii
        | Look::WordStartHalfAscii
        | Look::WordEndHalfAscii => run.min > 0 && run.class.is_word(false),
        Look::WordUnicode
        | Look::WordStartUnicode
        | Look::WordEndUnicode
        | Look::WordStartHalfUnicode
        | Look::WordEndHalfUnicode => run.min > 0 && run.class.is_word(true),
        _ => false,
    }
}

/// The classes that can follow the consuming step at `position`, up to the
/// first step that cannot match nothing, and whether the match can end right
/// after it.
fn follow(steps: &[Step], consuming: &[usize], position: usize) -> Option<(Vec<Class>, bool)> {
    let mut classes = Vec::new();
    for &next in &consuming[position + 1..] {
        classes.push(steps[next].first()?);
        if steps[next].min_len() > 0 {
            return Some((classes, false));
        }
    }
    Some((classes, true))
}

/// Flatten `hir` into items, or `None` outside the linear shape.
fn flatten(
    hir: &Hir,
    items: &mut Vec<Item>,
    names: &mut Vec<(usize, Option<Box<str>>)>,
) -> Option<()> {
    match hir.kind() {
        HirKind::Empty => {}
        HirKind::Literal(hir::Literal(bytes)) => match items.last_mut() {
            Some(Item::Literal(previous)) => previous.extend_from_slice(bytes),
            _ => items.push(Item::Literal(bytes.to_vec())),
        },
        HirKind::Class(class) => {
            let run = Run {
                class: Class::from_hir(class),
                min: 1,
                max: 1,
                shortest: false,
                stops: None,
            };
            items.push(Item::Run(run, true));
        }
        HirKind::Look(hir_look) => items.push(Item::Look(look(*hir_look))),
        HirKind::Repetition(repetition) => {
            let class = match repetition.sub.kind() {
                HirKind::Class(class) => Class::from_hir(class),
                HirKind::Literal(hir::Literal(bytes)) => Class::from_literal(bytes)?,
                _ => return None,
            };
            let max = repetition.max.map_or(usize::MAX, |max| max as usize);
            if max == 0 {
                return None;
            }
            let stops = class.stops();
            let run = Run {
                class,
                min: repetition.min as usize,
                max,
                shortest: false,
                stops,
            };
            items.push(Item::Run(run, repetition.greedy));
        }
        HirKind::Capture(capture) => {
            let index = capture.index as usize;
            names.push((index, capture.name.clone()));
            items.push(Item::Save(2 * index));
            flatten(&capture.sub, items, names)?;
            items.push(Item::Save(2 * index + 1));
        }
        HirKind::Concat(subs) => {
            for sub in subs {
                flatten(sub, items, names)?;
            }
        }
        HirKind::Alternation(_) => return None,
    }
    Some(())
}

impl LinearPlan {
    /// The plan for `hir`, or `None` when its matches are not one
    /// deterministic, linear path. With `prefilter`, a plan starting with a
    /// short fixed sequence of classes looks for its prefixes with a
    /// regex-automata prefilter, as the meta engine would.
    pub(crate) fn new(hir: &Hir, prefilter: bool) -> Option<LinearPlan> {
        // Unicode word boundaries without their data tables: the engines
        // refuse to build, and the look-around matcher would panic.
        if hir.properties().look_set().contains_word_unicode() {
            LookSet::singleton(Look::WordUnicode).available().ok()?;
        }
        let mut items = Vec::new();
        let mut names = Vec::new();
        flatten(hir, &mut items, &mut names)?;
        let consuming = |item: &Item| matches!(item, Item::Literal(_) | Item::Run(..));
        let first = items.iter().position(consuming)?;
        let last = items.iter().rposition(consuming)?;
        let mut leading = Vec::new();
        let mut trailing = Vec::new();
        let mut steps = Vec::new();
        let mut greedy = Vec::new();
        for (index, item) in items.into_iter().enumerate() {
            match item {
                Item::Look(look) if index < first => leading.push(look),
                Item::Look(look) if index > last => trailing.push(look),
                Item::Look(_) => return None,
                Item::Literal(bytes) => {
                    steps.push(Step::Literal(bytes.into_boxed_slice()));
                    greedy.push(true);
                }
                Item::Run(run, is_greedy) => {
                    steps.push(Step::Run(run));
                    greedy.push(is_greedy);
                }
                Item::Save(slot) => {
                    steps.push(Step::Save(slot));
                    greedy.push(true);
                }
            }
        }
        let start_only = leading.contains(&Look::Start);
        // A reverse search from the end finds these matches sooner.
        if trailing.contains(&Look::End) && !start_only {
            return None;
        }
        let consuming: Vec<usize> = (0..steps.len())
            .filter(|&i| !matches!(steps[i], Step::Save(_)))
            .collect();
        let (&first, &last) = (consuming.first()?, consuming.last()?);
        if steps[first].min_len() == 0 {
            return None;
        }
        let first_class = steps[first].first()?;
        for (position, &index) in consuming.iter().enumerate() {
            let Step::Run(run) = &steps[index] else {
                continue;
            };
            if run.min == run.max {
                continue;
            }
            let (follow, can_end) = follow(&steps, &consuming, position)?;
            if follow.iter().any(|class| class.overlaps(&run.class)) {
                return None;
            }
            if can_end && !trailing.is_empty() && !trailing.iter().any(|&look| ends_run(look, run))
            {
                return None;
            }
            let fails_after = !can_end || !trailing.is_empty();
            if index != first && fails_after && run.class.overlaps(&first_class) {
                return None;
            }
            let shortest = can_end && trailing.is_empty() && !greedy[index];
            if let Step::Run(run) = &mut steps[index] {
                run.shortest = shortest;
            }
        }
        let can_fail = consuming.len() > 1
            || !leading.is_empty()
            || !trailing.is_empty()
            || !matches!(steps[first], Step::Literal(_));
        let rescan = consuming
            .iter()
            .filter_map(|&index| steps[index].max_len())
            .fold(0, usize::saturating_add);
        if can_fail && rescan > RESCAN_BOUND {
            return None;
        }
        let decisive = if trailing.is_empty() {
            consuming
                .iter()
                .rposition(|&index| steps[index].min_len() > 0)
                .map_or(last, |position| consuming[position])
                + 1
        } else {
            steps.len()
        };
        let required = consuming[1..]
            .iter()
            .filter_map(|&index| match &steps[index] {
                Step::Literal(bytes) => Some(bytes),
                Step::Run(_) | Step::Save(_) => None,
            })
            // The last of the longest, like the suffix a reverse search uses.
            .max_by_key(|bytes| bytes.len())
            .map(|bytes| Box::new(memchr::memmem::Finder::new(bytes).into_owned()));
        // A literal after steps of fixed byte length sits at a fixed offset
        // from every match start, so its occurrences give the candidates.
        let mut offset = 0;
        let mut inner: Option<(usize, &[u8])> = None;
        for &index in &consuming {
            match &steps[index] {
                Step::Literal(bytes) => {
                    if index != first && inner.is_none_or(|(_, best)| bytes.len() > best.len()) {
                        inner = Some((offset, bytes));
                    }
                    offset += bytes.len();
                }
                Step::Run(run) if run.min == run.max && run.class.max_len() == 1 => {
                    offset += run.min;
                }
                Step::Run(_) | Step::Save(_) => break,
            }
        }
        let candidates = match (&steps[first], inner) {
            (Step::Run(_), Some((offset, bytes))) => Candidates::Offset(
                Box::new(memchr::memmem::Finder::new(bytes).into_owned()),
                offset,
            ),
            (step, _) => match step {
                Step::Literal(bytes) => {
                    Candidates::Literal(Box::new(memchr::memmem::Finder::new(bytes).into_owned()))
                }
                Step::Run(run) => {
                    let prefilter = if prefilter && run.min == run.max {
                        Prefilter::from_hir_prefix(MatchKind::LeftmostFirst, hir)
                            .filter(Prefilter::is_fast)
                    } else {
                        None
                    };
                    let first_bytes = run.class.first_bytes();
                    let mut bytes = (0..=255u8).filter(|&b| has_byte(&first_bytes, b));
                    match (prefilter, Stops::of(&mut bytes)) {
                        (Some(prefilter), _) => Candidates::Prefilter(prefilter),
                        (None, Some(stops)) => Candidates::Bytes(stops),
                        (None, None) => match ByteRanges::of(&first_bytes) {
                            Some(ranges) => Candidates::Ranges(ranges, run.class.clone()),
                            None => Candidates::Class(run.class.clone()),
                        },
                    }
                }
                Step::Save(_) => return None,
            },
        };
        let skip_first_run = matches!(&steps[first], Step::Run(run) if run.max == usize::MAX);
        let window = !matches!(candidates, Candidates::Offset(..))
            && match &steps[first] {
                // Retrying shorter runs costs little more than a window.
                Step::Run(run) if run.max != usize::MAX && run.max > 2 && !run.shortest => {
                    let (follow, can_end) = follow(&steps, &consuming, 0)?;
                    (!can_end || !trailing.is_empty())
                        && !follow.iter().any(|class| class.overlaps(&run.class))
                        && (!can_end || trailing.iter().any(|&look| ends_run(look, run)))
                }
                Step::Literal(_) | Step::Run(_) | Step::Save(_) => false,
            };
        let nfa_units = nfa_bound(hir);
        let mut group_names = Vec::new();
        group_names.resize(hir.properties().explicit_captures_len() + 1, None);
        for (index, name) in names {
            *group_names.get_mut(index)? = name;
        }
        let group_info = GroupInfo::new([group_names]).ok()?;
        Some(LinearPlan {
            steps: steps.into_boxed_slice(),
            leading: leading.into_boxed_slice(),
            trailing: trailing.into_boxed_slice(),
            candidates,
            required,
            start_only,
            skip_first_run,
            first,
            window,
            decisive,
            look_matcher: LookMatcher::new(),
            group_info,
            nfa_units,
        })
    }

    /// [`nfa_bound`] of the pattern, compared with [`NFA_UNIT_BUDGET`].
    pub(crate) fn nfa_units(&self) -> usize {
        self.nfa_units
    }

    /// Capture storage laid out as the meta engine's for the same pattern.
    pub(crate) fn create_captures(&self) -> RaCaptures {
        RaCaptures::all(self.group_info.clone())
    }

    /// The number of capture groups, including the whole match.
    pub(crate) fn captures_len(&self) -> usize {
        self.group_info.group_len(PatternID::ZERO)
    }

    /// The number of capture slots, two per group.
    pub(crate) fn slot_len(&self) -> usize {
        self.group_info.slot_len()
    }

    /// The first position in `from..end` where the first consuming step can
    /// match.
    #[inline]
    fn next_candidate(&self, haystack: &[u8], from: usize, end: usize) -> Option<usize> {
        let found = match &self.candidates {
            Candidates::Literal(finder) => finder.find(&haystack[from..end]),
            Candidates::Prefilter(prefilter) => {
                return prefilter
                    .find(haystack, Span { start: from, end })
                    .map(|span| span.start);
            }
            Candidates::Bytes(bytes) => bytes.find(&haystack[from..end]),
            Candidates::Offset(finder, offset) => {
                let begin = from + offset;
                return haystack
                    .get(begin..end)
                    .and_then(|rest| finder.find(rest))
                    .map(|at| from + at);
            }
            Candidates::Ranges(ranges, class) => {
                let mut at = from;
                while let Some(found) = ranges.find(haystack, at, end) {
                    if haystack[found] < 0x80 || class.match_len(haystack, found, end).is_some() {
                        return Some(found);
                    }
                    at = found + 1;
                }
                return None;
            }
            Candidates::Class(Class::Bytes(set)) => {
                haystack[from..end].iter().position(|&b| has_byte(set, b))
            }
            Candidates::Class(class @ Class::Chars(set)) => {
                let mut at = from;
                while at < end {
                    let b = haystack[at];
                    if b < 0x80 {
                        if bit128(set.ascii, b) {
                            return Some(at);
                        }
                    } else if class.match_len(haystack, at, end).is_some() {
                        return Some(at);
                    }
                    at += 1;
                }
                return None;
            }
        };
        found.map(|offset| from + offset)
    }

    /// The end of the maximal run from `at`, up to `run.max` members, and how
    /// many members it has, counted only up to one when the run is unbounded
    /// and ends at a stop byte.
    #[inline]
    fn scan(run: &Run, haystack: &[u8], at: usize, end: usize, utf8: bool) -> (usize, usize) {
        if let Some(stops) = run.stops
            && utf8
            && run.max == usize::MAX
            && run.min <= 1
            && char_boundary(haystack, at)
            && char_boundary(haystack, end)
        {
            let stop = stops
                .find(&haystack[at..end])
                .map_or(end, |offset| at + offset);
            return (stop, usize::from(stop > at));
        }
        LinearPlan::take(run, haystack, at, end, run.max)
    }

    /// The end of the run from `at` of at most `limit` members, and how many
    /// it has.
    #[inline]
    fn take(run: &Run, haystack: &[u8], at: usize, end: usize, limit: usize) -> (usize, usize) {
        let haystack = &haystack[..end];
        let mut pos = at;
        let mut count = 0;
        match &run.class {
            Class::Bytes(set) => {
                while count < limit
                    && let Some(&b) = haystack.get(pos)
                    && has_byte(set, b)
                {
                    pos += 1;
                    count += 1;
                }
            }
            Class::Chars(set) => {
                while count < limit
                    && let Some(&b) = haystack.get(pos)
                {
                    if b < 0x80 {
                        if !bit128(set.ascii, b) {
                            break;
                        }
                        pos += 1;
                    } else {
                        match set.non_ascii_len(haystack, pos, end) {
                            Some(len) => pos += len,
                            None => break,
                        }
                    }
                    count += 1;
                }
            }
        }
        (pos, count)
    }

    /// Whether `look` holds at `at`, deciding the common assertions from the
    /// neighbouring bytes as regex-automata does.
    #[inline]
    fn look_at(&self, look: Look, haystack: &[u8], at: usize) -> bool {
        // NUL, a non-word byte, stands in beyond either end.
        let before = if at == 0 { 0 } else { haystack[at - 1] };
        let after = haystack.get(at).copied().unwrap_or(0);
        let word = |b: u8| WORD_BYTE[usize::from(b)];
        let (before, after) = match look {
            Look::Start => return at == 0,
            Look::End => return at == haystack.len(),
            Look::StartLF => return at == 0 || before == b'\n',
            Look::EndLF => return at == haystack.len() || after == b'\n',
            Look::WordAscii
            | Look::WordAsciiNegate
            | Look::WordStartAscii
            | Look::WordEndAscii
            | Look::WordStartHalfAscii
            | Look::WordEndHalfAscii => (word(before), word(after)),
            Look::WordUnicode
            | Look::WordUnicodeNegate
            | Look::WordStartUnicode
            | Look::WordEndUnicode
            | Look::WordStartHalfUnicode
            | Look::WordEndHalfUnicode => {
                // An ASCII byte is a whole character on either side.
                if (before | after) >= 0x80 {
                    return self.look_matcher.matches(look, haystack, at);
                }
                (word(before), word(after))
            }
            Look::StartCRLF | Look::EndCRLF => {
                return self.look_matcher.matches(look, haystack, at);
            }
        };
        match look {
            Look::WordAscii | Look::WordUnicode => before != after,
            Look::WordAsciiNegate | Look::WordUnicodeNegate => before == after,
            Look::WordStartAscii | Look::WordStartUnicode => !before && after,
            Look::WordEndAscii | Look::WordEndUnicode => before && !after,
            Look::WordStartHalfAscii | Look::WordStartHalfUnicode => !before,
            Look::WordEndHalfAscii | Look::WordEndHalfUnicode => !after,
            _ => unreachable!("only word assertions reach here"),
        }
    }

    /// Match the steps anchored at `start`, recording capture slots when
    /// given. A membership test only runs the decisive steps. On failure,
    /// [`LOOK_MISS`] if a leading assertion failed, else where the first run
    /// ended, or `start` when the first step is a literal.
    #[inline]
    fn attempt<S: Slots + ?Sized>(
        &self,
        input: &PlanInput<'_>,
        start: usize,
        earliest: bool,
        mut slots: Option<&mut S>,
    ) -> Result<usize, usize> {
        let (haystack, end) = (input.haystack, input.end);
        if !self
            .leading
            .iter()
            .all(|&look| self.look_at(look, haystack, start))
        {
            return Err(LOOK_MISS);
        }
        let steps = if earliest {
            &self.steps[..self.decisive]
        } else {
            &self.steps[..]
        };
        let mut pos = start;
        let mut first_run_end = start;
        for (index, step) in steps.iter().enumerate() {
            match step {
                Step::Literal(bytes) => {
                    if !haystack
                        .get(pos..end)
                        .is_some_and(|rest| rest.starts_with(bytes))
                    {
                        return Err(first_run_end);
                    }
                    pos += bytes.len();
                }
                Step::Run(run) => {
                    let decided =
                        earliest && index + 1 == self.decisive && self.trailing.is_empty();
                    let (stop, count) = if run.shortest || decided {
                        LinearPlan::take(run, haystack, pos, end, run.min)
                    } else {
                        LinearPlan::scan(run, haystack, pos, end, input.utf8)
                    };
                    if index == self.first {
                        first_run_end = stop;
                    }
                    if count < run.min {
                        return Err(first_run_end);
                    }
                    pos = stop;
                }
                Step::Save(slot) => {
                    if let Some(slots) = slots.as_deref_mut() {
                        slots.set(*slot, pos);
                    }
                }
            }
        }
        if !self
            .trailing
            .iter()
            .all(|&look| self.look_at(look, haystack, pos))
        {
            return Err(first_run_end);
        }
        Ok(pos)
    }

    /// The leftmost-first match among the starts inside the maximal run of
    /// the first step's class from `candidate`, or where to look next.
    fn window_attempt<S: Slots + ?Sized>(
        &self,
        input: &PlanInput<'_>,
        candidate: usize,
        earliest: bool,
        mut slots: Option<&mut S>,
    ) -> Result<(usize, usize), usize> {
        let Step::Run(run) = &self.steps[self.first] else {
            unreachable!("a window plan starts with a run");
        };
        let (haystack, end) = (input.haystack, input.end);
        let (run_end, total) = LinearPlan::take(run, haystack, candidate, end, usize::MAX);
        let resume = run_end.max(candidate + 1);
        // A start leaving more than `max` members after it stops before
        // another member, which nothing after the run can continue.
        let mut start = candidate;
        for _ in 0..total.saturating_sub(run.max) {
            start += run.class.match_len(haystack, start, end).unwrap_or(1);
        }
        let mut left = total.min(run.max);
        while left >= run.min {
            match self.attempt(input, start, earliest, slots.as_deref_mut()) {
                Ok(match_end) => return Ok((start, match_end)),
                Err(LOOK_MISS) => {
                    start += run.class.match_len(haystack, start, end).unwrap_or(1);
                    left -= 1;
                }
                // Every start here reaches the same end of the run.
                Err(_) => break,
            }
        }
        Err(resume)
    }

    /// The leftmost-first match in `input`, filling every capture slot
    /// (start and end per group) when given.
    #[inline(always)]
    pub(crate) fn find<S: Slots + ?Sized>(
        &self,
        input: &PlanInput<'_>,
        mut slots: Option<&mut S>,
    ) -> Option<(usize, usize)> {
        let found = self.search(input, false, slots.as_deref_mut())?;
        if let Some(slots) = slots {
            slots.set(0, found.0);
            slots.set(1, found.1);
        }
        Some(found)
    }

    /// Whether `find` would find a match, without finding where it ends.
    #[inline]
    pub(crate) fn is_match(&self, input: &PlanInput<'_>) -> bool {
        self.search::<[usize]>(input, true, None).is_some()
    }

    #[inline(always)]
    fn search<S: Slots + ?Sized>(
        &self,
        input: &PlanInput<'_>,
        earliest: bool,
        mut slots: Option<&mut S>,
    ) -> Option<(usize, usize)> {
        let start = input.start;
        if input.anchored || self.start_only {
            let found = self.attempt(input, start, earliest, slots);
            return found.ok().map(|match_end| (start, match_end));
        }
        if let Some(required) = &self.required
            && required.find(&input.haystack[start..input.end]).is_none()
        {
            return None;
        }
        if self.window {
            return self.search_windows(input, earliest, slots);
        }
        let mut from = start;
        loop {
            let candidate = self.next_candidate(input.haystack, from, input.end)?;
            let resume = match self.attempt(input, candidate, earliest, slots.as_deref_mut()) {
                Ok(match_end) => return Some((candidate, match_end)),
                // Every later start inside an unbounded first run ends it at
                // the same place, and so fails the same way.
                Err(run_end) if self.skip_first_run && run_end != LOOK_MISS => {
                    run_end.max(candidate + 1)
                }
                Err(_) => candidate + 1,
            };
            if resume >= input.end {
                return None;
            }
            from = resume;
        }
    }

    /// `search` for a plan whose first step is a bounded run, one maximal
    /// run of its class at a time.
    #[inline(never)]
    fn search_windows<S: Slots + ?Sized>(
        &self,
        input: &PlanInput<'_>,
        earliest: bool,
        mut slots: Option<&mut S>,
    ) -> Option<(usize, usize)> {
        let mut from = input.start;
        loop {
            let candidate = self.next_candidate(input.haystack, from, input.end)?;
            match self.window_attempt(input, candidate, earliest, slots.as_deref_mut()) {
                Ok(found) => return Some(found),
                Err(resume) if resume >= input.end => return None,
                Err(resume) => from = resume,
            }
        }
    }
}

/// An attempt's failure at a leading assertion, which says nothing about
/// later starts.
const LOOK_MISS: usize = usize::MAX;

/// Where a plan searches: `start..end` of `haystack`, which is valid UTF-8
/// when `utf8` is set, only at `start` when `anchored`. Assertions see the
/// whole haystack.
pub(crate) struct PlanInput<'h> {
    pub(crate) haystack: &'h [u8],
    pub(crate) start: usize,
    pub(crate) end: usize,
    pub(crate) anchored: bool,
    pub(crate) utf8: bool,
}

/// Capture slot storage a plan fills: regex-automata's, or the VM's layout.
pub(crate) trait Slots {
    fn set(&mut self, slot: usize, at: usize);
}

impl Slots for [Option<NonMaxUsize>] {
    fn set(&mut self, slot: usize, at: usize) {
        self[slot] = NonMaxUsize::new(at);
    }
}

impl Slots for [usize] {
    fn set(&mut self, slot: usize, at: usize) {
        self[slot] = at;
    }
}

#[cfg(test)]
mod tests {
    use super::{LinearPlan, NFA_UNIT_BUDGET};

    fn plan(pattern: &str) -> Option<LinearPlan> {
        LinearPlan::new(&regex_syntax::parse(pattern).unwrap(), true)
    }

    /// Shapes the plan takes, and those it leaves to an engine: overlapping
    /// follows, assertions a shorter run could satisfy, rescans beyond the
    /// bound, and end anchors a reverse search finds sooner.
    #[test]
    fn admission_follows_shape_rules() {
        for pattern in [
            r"\w+",
            r#""[^"]*""#,
            r"\bfn\s+(\w+)",
            r"\b\w+\b",
            r"^\d+$",
            r"(?m)\w+$",
            r"a\w*?",
            r"\d{4}-\d{2}-\d{2}",
            "a{1,3}b*c",
            r"\w+:\s*\w+",
        ] {
            assert!(plan(pattern).is_some(), "{pattern}");
        }
        for pattern in [
            r"\w+s",
            r"\w+\B",
            r"(?m)[^a]+$",
            "[a-z]+?x",
            r"\d+$",
            r"\w*",
            "ab|cd",
            "(?:ab)+",
            "a{1,70}b",
            r"\w+-\w+-x",
            r"x[a-z]*\d",
        ] {
            assert!(plan(pattern).is_none(), "{pattern}");
        }
    }

    /// The word-at-a-time test marks exactly the bytes in the set, in every
    /// lane and whatever the neighbouring lanes hold.
    #[test]
    fn byte_ranges_mask_each_lane_exactly() {
        let sets: [&[u8]; 5] = [b"AZ", b"09af", b"az", b"\x00\x10", b"\x7F\x7F"];
        for (index, bounds) in sets.iter().enumerate() {
            let mut bytes = [0u64; 4];
            for pair in bounds.chunks(2) {
                for b in pair[0]..=pair[1] {
                    super::insert_byte(&mut bytes, b);
                }
            }
            if index % 2 == 1 {
                super::insert_byte(&mut bytes, 0xC3);
            }
            let ranges = super::ByteRanges::of(&bytes).unwrap();
            for b in 0..=255u8 {
                for lane in 0..8 {
                    for filler in [0u8, 0x7F, 0x80, 0xFF, b'A', b'z'] {
                        let mut word = [filler; 8];
                        word[lane] = b;
                        let mask = ranges.mask(u64::from_le_bytes(word));
                        let expected = word.iter().enumerate().fold(0u64, |mask, (i, &w)| {
                            mask | (u64::from(ranges.contains(w)) << (8 * i + 7))
                        });
                        assert_eq!(mask, expected, "{bounds:?} {b} {lane} {filler}");
                    }
                }
            }
        }
    }

    /// The direct decoder accepts and rejects exactly what the standard
    /// library does, for every lead byte and the boundary continuations.
    #[test]
    fn decode_wide_matches_from_utf8() {
        let seconds = [0x7F, 0x80, 0x8F, 0x90, 0x9F, 0xA0, 0xBF, 0xC0];
        for lead in 0xC0..=0xFFu8 {
            for &b1 in &seconds {
                for &b2 in &[0x7F, 0x80, 0xBF, 0xC0] {
                    for &b3 in &[0x7F, 0x80, 0xBF, 0xC0] {
                        let bytes = [lead, b1, b2, b3];
                        let expected = (3..=4).find_map(|len| {
                            core::str::from_utf8(&bytes[..len])
                                .ok()
                                .and_then(|text| text.chars().next().map(|c| (c, len)))
                                .filter(|_| lead >= 0xE0)
                        });
                        let expected = expected.filter(|&(c, len)| c.len_utf8() == len);
                        assert_eq!(super::decode_wide(&bytes), expected, "{bytes:02X?}");
                        assert_eq!(
                            super::decode_wide(&bytes[..3]),
                            expected.filter(|&(_, l)| l == 3)
                        );
                    }
                }
            }
        }
    }

    /// The per-range bound never undercounts the UTF-8 sequences the
    /// regex-automata compiler builds a class from.
    #[test]
    fn sequence_bound_covers_utf8_sequences() {
        use regex_syntax::hir::{Class, HirKind};
        use regex_syntax::utf8::Utf8Sequences;
        for pattern in [
            r"\w",
            r"\p{L}",
            r"\d",
            "(?s:.)",
            r"[^\n]",
            r"\s",
            "(?i:k)",
            r"\p{Han}",
            "[€-𝄞]",
            r"[\x{7FF}-\x{800}]",
            r"[\x{D7FF}-\x{E000}]",
            r"[\x{FFFF}-\x{10000}]",
        ] {
            let hir = regex_syntax::parse(pattern).unwrap();
            let HirKind::Class(Class::Unicode(class)) = hir.kind() else {
                panic!("{pattern} is not a Unicode class");
            };
            for range in class.iter() {
                let actual = Utf8Sequences::new(range.start(), range.end()).count();
                let bound = super::sequence_bound(range.start(), range.end());
                assert!(actual <= bound, "{pattern} {range:?}: {actual} > {bound}");
            }
        }
    }

    /// Every component the default NFA size limit counts takes units in
    /// proportion. Each family below is repeated as often as it takes for
    /// 0.1.5 to reject it with that limit (measured once with regex-automata
    /// 0.4.18, the locked version). There, its bound is over sixteen times the
    /// budget, so no pattern the limit rejects can skip building the engine.
    #[test]
    fn nfa_bound_exceeds_the_budget_wherever_the_default_limit_rejects() {
        let families: &[(&str, &str, &str, usize)] = &[
            ("", "()", "a", 109_225),
            ("", "(a)", "", 109_225),
            ("", "(?m:^)", "a", 327_673),
            ("a", "(?m:$)", "", 327_673),
            ("", r"\b", "a", 327_673),
            ("", "a", "", 327_672),
            ("", "(?s:.)", "", 10_878),
            ("", r"\w", "", 210),
            ("", "a?", "", 100_823),
            ("", "a*b", "", 100_823),
            ("", r"(\w)", "", 210),
            ("", r"[\x{10000}-\x{10FFFF}]", "", 22_795),
            ("", r"\p{Han}", "", 2_891),
            ("", "[€-𝄞]", "", 12_603),
            ("", "(?i:[a-z])", "", 34_493),
            ("", r"[\x{80}-\x{7FF}]", "", 79_437),
        ];
        for &(prefix, unit, suffix, rejected) in families {
            let bound = |copies: usize| {
                let pattern = format!("{prefix}{}{suffix}", unit.repeat(copies));
                super::nfa_bound(&regex_syntax::parse(&pattern).unwrap())
            };
            // The bound grows by the same amount with every copy.
            let (hundred, two_hundred) = (bound(100), bound(200));
            let at_rejection = hundred + (two_hundred - hundred) * (rejected - 100) / 100;
            assert!(
                at_rejection > 16 * NFA_UNIT_BUDGET,
                "{unit}: {at_rejection} units where the limit rejects"
            );
        }
    }

    /// Typical plans stay within the budget that lets them skip building the
    /// meta engine; repeated copies of large classes do not.
    #[test]
    fn nfa_units_bound_typical_patterns() {
        let units = |pattern: &str| plan(pattern).unwrap().nfa_units();
        assert!(units(r"\b\w+\b") <= NFA_UNIT_BUDGET);
        assert!(units(r"\bfn\s+(\w+)") <= NFA_UNIT_BUDGET);
        assert!(units(r#""[^"]*""#) <= NFA_UNIT_BUDGET);
        assert!(units(r"\w{8}") > NFA_UNIT_BUDGET);
        assert!(units(&"ab".repeat(5000)) > NFA_UNIT_BUDGET);
    }
}
