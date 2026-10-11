// Copyright 2026 Notes Supply AB. Distributed under the MIT license.
//! The characters a plan step consumes: Unicode scalar values as UTF-8, or
//! single bytes, with the byte-level scans candidates use.

use alloc::boxed::Box;
use alloc::vec::Vec;
use regex_automata::util::lazy::Lazy;
use regex_syntax::hir::{self, HirKind};

use crate::vm::{char_at, range_contains};

#[derive(Clone, Copy, Debug)]
pub(super) enum Stops {
    None,
    One(u8),
    Two(u8, u8),
    Three(u8, u8, u8),
}

/// The characters a step can consume, one at a time.
#[derive(Clone, Debug)]
pub(super) enum Class {
    /// Unicode scalar values, as UTF-8. Invalid UTF-8 is never a member.
    Chars(Box<CharSet>),
    /// Single bytes.
    Bytes([u64; 4]),
}

#[derive(Clone, Debug)]
pub(super) struct CharSet {
    pub(super) ascii: u128,
    /// U+0080 to U+07FF, which encode in two bytes, one bit each.
    pub(super) two: [u64; 30],
    /// Sorted inclusive ranges from U+0800.
    pub(super) wide: Box<[(char, char)]>,
}

/// Bytes in at most two ASCII ranges, and any non-ASCII byte when `high` is
/// set, tested eight at a time within a 64-bit word.
#[derive(Clone, Copy, Debug)]
pub(super) struct ByteRanges {
    /// Inclusive ranges within ASCII; unused ones are empty.
    pub(super) ranges: [(u8, u8); 2],
    pub(super) high: bool,
    /// The same bytes, one bit each, for testing them one at a time.
    pub(super) bytes: [u64; 4],
}

impl ByteRanges {
    /// The ranges holding every byte of `bytes`, if there are few enough.
    pub(super) fn of(bytes: &[u64; 4]) -> Option<ByteRanges> {
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
    pub(super) fn contains(&self, b: u8) -> bool {
        has_byte(&self.bytes, b)
    }

    /// The first position in `from..end` holding a byte in the set.
    #[inline]
    pub(super) fn find(&self, haystack: &[u8], from: usize, end: usize) -> Option<usize> {
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
pub(super) static WORD_BYTE: [bool; 256] = {
    let mut table = [false; 256];
    let mut b = 0;
    while b < 128 {
        table[b] = (ASCII_WORD >> b) & 1 == 1;
        b += 1;
    }
    table
};

pub(super) fn has_byte(set: &[u64; 4], b: u8) -> bool {
    set[usize::from(b >> 6)] & (1 << (b & 63)) != 0
}

pub(super) fn insert_byte(set: &mut [u64; 4], b: u8) {
    set[usize::from(b >> 6)] |= 1 << (b & 63);
}

/// Bit `b` of `bits`, for `b` below 128, read from one 64-bit half: a
/// variable 128-bit shift costs several instructions per character.
#[inline]
pub(super) fn bit128(bits: u128, b: u8) -> bool {
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
pub(super) fn char_boundary(haystack: &[u8], at: usize) -> bool {
    haystack.get(at).is_none_or(|&b| (b as i8) >= -0x40)
}

impl CharSet {
    pub(super) fn new(ranges: impl IntoIterator<Item = (char, char)>) -> CharSet {
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
    pub(super) fn non_ascii_len(&self, haystack: &[u8], at: usize, end: usize) -> Option<usize> {
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
pub(super) fn class_units(class: &hir::Class) -> usize {
    match class {
        hir::Class::Unicode(class) => class
            .iter()
            .map(|range| sequence_bound(range.start(), range.end()))
            .fold(0, usize::saturating_add),
        hir::Class::Bytes(class) => class.ranges().len(),
    }
}

impl Class {
    pub(super) fn from_hir(class: &hir::Class) -> Class {
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
    pub(super) fn from_literal(bytes: &[u8]) -> Option<Class> {
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
    pub(super) fn match_len(&self, haystack: &[u8], at: usize, end: usize) -> Option<usize> {
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

    pub(super) fn has_ascii(&self, b: u8) -> bool {
        match self {
            Class::Chars(set) => bit128(set.ascii, b),
            Class::Bytes(set) => has_byte(set, b),
        }
    }

    /// The most bytes one member takes.
    pub(super) fn max_len(&self) -> usize {
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
    pub(super) fn stops(&self) -> Option<Stops> {
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
    pub(super) fn first_bytes(&self) -> [u64; 4] {
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
    pub(super) fn overlaps(&self, other: &Class) -> bool {
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
    pub(super) fn is_word(&self, unicode: bool) -> bool {
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
    pub(super) fn of(bytes: &mut impl Iterator<Item = u8>) -> Option<Stops> {
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
    pub(super) fn find(self, haystack: &[u8]) -> Option<usize> {
        match self {
            Stops::None => None,
            Stops::One(a) => memchr::memchr(a, haystack),
            Stops::Two(a, b) => memchr::memchr2(a, b, haystack),
            Stops::Three(a, b, c) => memchr::memchr3(a, b, c, haystack),
        }
    }
}

#[cfg(test)]
mod tests {
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
}
