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
use regex_automata::util::look::{Look, LookMatcher, LookSet};
use regex_automata::util::prefilter::Prefilter;
use regex_automata::util::primitives::NonMaxUsize;
use regex_automata::{MatchKind, PatternID, Span};
use regex_syntax::hir::{self, Hir, HirKind};

use crate::vm::char_at;

mod class;

use class::{ByteRanges, Class, Stops, WORD_BYTE, bit128, char_boundary, class_units, has_byte};

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
