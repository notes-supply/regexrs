// Copyright 2026 Notes Supply AB. Distributed under the MIT license.
//! Plans: one-pass forward search for regular patterns whose matches follow
//! one deterministic path from each start.
//!
//! A plan is a program of literals, single-character runs, ordered
//! alternations and optional or repeated groups, with capture boundaries
//! anywhere and look-around assertions only at its ends. It is admitted only
//! where the next character decides every choice. The characters that can
//! begin each alternative, each further character of a run and each further
//! repetition of a group are disjoint from those that can follow instead, and
//! no alternative or repeated group can match nothing. Greedy and lazy
//! priorities then differ only where the rest of the pattern can match
//! nothing. There a lazy choice stops; a greedy one continues, after recording
//! the position, and the capture slots that what follows could change, as the
//! match to report if the continuation fails. That record is the last match a
//! one-pass DFA keeps.
//!
//! An attempt therefore reads forward only. It never returns to an earlier
//! position or tries another alternative, and no search is handed to another
//! engine. Assertions after a run or group stop the match only where its end
//! is already decided. Patterns outside this shape keep their regex-automata
//! engine, which no search through a plan consults.
//!
//! A pattern whose prefixes are exactly its matches, a finite set of literals
//! without captures or assertions, needs no program at all. Its leftmost-first
//! prefilter reports the matches in the pattern's order of preference, as the
//! meta engine's literal strategy does, and so admission does not apply to it.
//!
//! Work stays linear in the haystack. Candidate starts are tried left to
//! right. Between an attempt's start, or the last match it recorded, and any
//! point where it can fail, it reads at most [`RESCAN_BOUND`] bytes where
//! another attempt could start. An unbounded first run, which a failed attempt
//! skips, and unbounded runs or repetitions holding no character a match can
//! start with do not count. So each byte is read by a bounded number of
//! attempts, and the bytes read past a reported match obey the same bound,
//! which keeps whole iterations linear as well. A bounded first run is tried
//! once per maximal run of its class, from the starts that can reach its end.
//!
//! Candidates come from the first literal, a literal at a fixed offset, a
//! regex-automata prefilter, memchr over at most three bytes, a word-at-a-time
//! test of byte ranges or a class scan, in that order of preference.

use alloc::borrow::Cow;
use alloc::boxed::Box;
use alloc::vec::Vec;
use regex_automata::util::captures::{Captures as RaCaptures, GroupInfo};
use regex_automata::util::look::{Look, LookMatcher, LookSet};
use regex_automata::util::prefilter::Prefilter;
use regex_automata::util::primitives::NonMaxUsize;
use regex_automata::{MatchKind, PatternID, Span};
use regex_syntax::hir::{self, Hir, HirKind, literal};

use crate::vm::char_at;

mod class;

use class::{
    ByteRanges, Class, Stops, WORD_BYTE, bit128, bytes_in, char_boundary, class_units, has_byte,
};

/// A regular pattern searched as one deterministic program of steps.
#[derive(Clone, Debug)]
pub(crate) struct Plan {
    steps: Box<[Step]>,
    /// Assertions tested where a candidate starts.
    leading: Box<[Look]>,
    /// Assertions tested where the last step ends.
    trailing: Box<[Look]>,
    /// Lookbehinds tested where a candidate starts and lookaheads where the
    /// last step ends, when there are any.
    edges: Option<Box<Edges>>,
    /// Some assertion or lookbehind is tested where an attempt starts.
    checks_start: bool,
    /// Some assertion or lookahead is tested where the last step ends.
    checks_end: bool,
    candidates: Candidates,
    /// The longest literal after the first step, which every match contains:
    /// a search where it does not occur ends at once.
    required: Option<Box<memchr::memmem::Finder<'static>>>,
    /// A leading `\A`: only the search's start can begin a match.
    start_only: bool,
    /// How a search proceeds, decided once.
    whole: Whole,
    /// The first consuming step is an unbounded run: an attempt that fails
    /// after it fails from every later start inside the same run.
    skip_first_run: bool,
    /// The index of the first consuming step.
    first: usize,
    /// A lookbehind fails inside the maximal run of the unbounded first
    /// step's class wherever it fails at the run's start: every alternative
    /// ends with a character outside that class.
    look_skip: bool,
    /// The first step is a bounded run that nothing after it can continue:
    /// within one maximal run of its class, only starts whose run reaches the
    /// end of it can match, and all of them continue alike from there.
    window: bool,
    /// How many steps decide whether a match exists. Every step after them
    /// can match nothing and no assertion follows, so a membership test stops
    /// there, and without trailing assertions its last run or group only
    /// needs its minimum.
    decisive: usize,
    /// The capture slots inside alternatives and groups, which a match need
    /// not set: an attempt clears them first.
    conditional: Box<[usize]>,
    /// The program has an alternation or a group.
    branching: bool,
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
    /// Ordered alternatives, of which the next character selects at most one.
    Alt(Box<Alt>),
    /// An optional or repeated group.
    Group(Box<Group>),
}

#[derive(Clone, Debug)]
struct Run {
    class: Class,
    min: usize,
    /// `usize::MAX` when unbounded.
    max: usize,
    lazy: bool,
    /// Consume `min` characters only: a run of fixed length, or a lazy run
    /// that nothing after it can fail. Every other run consumes as much as it
    /// can, which is also what a lazy run does when the next character must
    /// end it.
    shortest: bool,
    /// For a class of every scalar value but at most three ASCII bytes, those
    /// bytes: in valid UTF-8 the run ends at the next of them.
    stops: Option<Stops>,
}

#[derive(Clone, Debug)]
struct Alt {
    branches: Box<[Box<[Step]>]>,
    /// The characters each branch starts with, pairwise disjoint.
    firsts: Box<[Class]>,
    /// The branch each byte selects, or [`NO_BRANCH`], when no two branches
    /// start with the same byte.
    by_byte: Option<Box<[u8; 256]>>,
    /// The characters any branch starts with.
    first: Class,
}

/// How a plan searches.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Whole {
    /// Attempts at candidate starts.
    Attempts,
    /// The pattern can match nothing, so a match starts wherever a search
    /// does, unless it would end inside a character.
    Empty,
    /// The candidates are the matches: the pattern is a finite set of
    /// literals, without captures or assertions, which its leftmost-first
    /// prefilter reports in the pattern's order of preference.
    Literals,
    /// The program is one unbounded run of at least one member, without
    /// captures or assertions: a match is the maximal run from the first
    /// candidate long enough.
    Run,
}

/// No branch of an alternation starts with this byte.
const NO_BRANCH: u8 = u8::MAX;

#[derive(Clone, Debug)]
struct Group {
    body: Box<[Step]>,
    min: usize,
    /// `usize::MAX` when unbounded.
    max: usize,
    greedy: bool,
    /// The characters the body starts with.
    first: Class,
    /// The rest of the pattern can match nothing after each repetition from
    /// the minimum on: a greedy group records each such position before
    /// repeating, and a lazy one stops there.
    tail: bool,
    /// The slots that what follows a recorded position could change.
    snapshot: Box<[usize]>,
    /// The slots the rest of the pattern sets when it matches nothing.
    completion: Box<[usize]>,
}

/// The largest [`Plan::nfa_units`] for which the meta engine's default
/// NFA size limit certainly accepts the pattern, so its engine need not be
/// built to find out. Against that limit, a unit of any component costs at
/// most about 66 bytes (astral classes), so a pattern within the budget needs
/// at most about 540 KiB of the 10 MiB allowed. Repeated components first
/// exceed the limit at about 160,000 units (astral classes), 200,000
/// (optional characters) and 327,000 (captures, assertions, literal bytes).
pub(crate) const NFA_UNIT_BUDGET: usize = 8192;

/// The most bytes where another attempt could start that an attempt may
/// read, since it started or last recorded a match, before it fails; each
/// position is then read by at most about this many attempts.
const RESCAN_BOUND: usize = 64;

/// The most capture slots a recorded match keeps.
const SNAPSHOT: usize = 16;

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
    /// Every match starts with a character of this class.
    Class(Class),
    /// Every match is preceded by one of a prefilter's literals, all of this
    /// many bytes: a lookbehind.
    Behind(Prefilter, usize),
    /// Every match starts with the maximal run of this class that ends where
    /// this literal, whose first character is outside the class, starts.
    Suffix(Box<memchr::memmem::Finder<'static>>, Class),
}

/// A look-around assertion at an edge of a plan, over a fixed number of
/// characters in each alternative: a lookbehind tested where an attempt
/// starts, or a lookahead where it ends. Testing one reads at most that many
/// characters next to the position and decides no choice.
#[derive(Clone, Debug)]
pub(crate) struct Edge {
    negated: bool,
    /// Each alternative's characters, in order.
    alternatives: Box<[Box<[Class]>]>,
}

/// The most characters an edge assertion may test in all.
const EDGE_CHARS: usize = 256;

/// Look-around assertions around a regular pattern.
#[derive(Clone, Debug, Default)]
pub(crate) struct Edges {
    pub(crate) behind: Vec<Edge>,
    pub(crate) ahead: Vec<Edge>,
}

impl Edge {
    /// The assertion `body` makes, negated if `negated`, or `None` unless each
    /// of its alternatives is a fixed sequence of characters, without
    /// captures or assertions.
    pub(crate) fn new(body: &Hir, negated: bool) -> Option<Edge> {
        let branches = match body.kind() {
            HirKind::Alternation(subs) => subs.iter().collect(),
            _ => Vec::from([body]),
        };
        let mut total = 0;
        let alternatives = branches
            .into_iter()
            .map(|branch| {
                let mut nodes = Vec::new();
                let mut names = Vec::new();
                flatten(branch, &mut nodes, &mut names)?;
                let mut classes = Vec::new();
                for node in nodes {
                    match node {
                        Node::Literal(bytes) => {
                            let mut at = 0;
                            while at < bytes.len() {
                                let len = char_at(&bytes, at).map_or(1, |(_, len)| len);
                                classes.push(Class::from_literal(&bytes[at..at + len])?);
                                at += len;
                            }
                        }
                        Node::Run(run) if run.min == run.max && run.min <= EDGE_CHARS => {
                            classes.extend(core::iter::repeat_n(run.class, run.min));
                        }
                        _ => return None,
                    }
                }
                total += classes.len();
                Some(classes.into_boxed_slice())
            })
            .collect::<Option<Vec<_>>>()?;
        (total <= EDGE_CHARS).then(|| Edge {
            negated,
            alternatives: alternatives.into_boxed_slice(),
        })
    }

    /// Whether the assertion holds as a lookbehind at `at`.
    #[inline]
    fn behind(&self, haystack: &[u8], at: usize) -> bool {
        let found = self.alternatives.iter().any(|alternative| {
            let mut pos = at;
            alternative.iter().rev().all(|class| {
                class
                    .len_before(haystack, pos)
                    .map(|len| pos -= len)
                    .is_some()
            })
        });
        found != self.negated
    }

    /// Whether the assertion holds as a lookahead at `at`.
    #[inline]
    fn ahead(&self, haystack: &[u8], at: usize) -> bool {
        let found = self.alternatives.iter().any(|alternative| {
            let mut pos = at;
            alternative.iter().all(|class| {
                class
                    .match_len(haystack, pos, haystack.len())
                    .map(|len| pos += len)
                    .is_some()
            })
        });
        found != self.negated
    }

    /// For a positive assertion with no empty alternative, the characters
    /// its alternatives end with.
    fn last_classes(&self) -> Option<Class> {
        if self.negated {
            return None;
        }
        let mut lasts = self
            .alternatives
            .iter()
            .map(|alternative| alternative.last());
        let first = lasts.next()??.clone();
        lasts.try_fold(first, |union, last| union.union(last?))
    }

    /// The literals of a positive assertion whose alternatives each hold
    /// single characters, when they all have the same non-zero byte length.
    fn literals(&self) -> Option<(Vec<Vec<u8>>, usize)> {
        if self.negated {
            return None;
        }
        let literals = self
            .alternatives
            .iter()
            .map(|alternative| {
                alternative
                    .iter()
                    .try_fold(Vec::new(), |mut literal, class| {
                        literal.extend(class.single()?);
                        Some(literal)
                    })
            })
            .collect::<Option<Vec<_>>>()?;
        let len = literals.first()?.len();
        (len > 0 && literals.iter().all(|literal| literal.len() == len)).then_some((literals, len))
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

/// A pattern element before admission.
enum Node {
    Look(Look),
    Literal(Vec<u8>),
    Run(Run),
    Save(usize),
    Alt(Vec<Vec<Node>>),
    Group {
        body: Vec<Node>,
        min: usize,
        max: usize,
        greedy: bool,
    },
}

impl Run {
    fn new(class: Class, min: usize, max: usize, greedy: bool) -> Run {
        let stops = if max == usize::MAX {
            class.stops()
        } else {
            None
        };
        Run {
            class,
            min,
            max,
            lazy: !greedy,
            shortest: min == max,
            stops,
        }
    }
}

impl Step {
    /// The step for `node`, or `None` for an assertion or a shape the
    /// program cannot take.
    fn from_node(node: Node) -> Option<Step> {
        Some(match node {
            Node::Look(_) => return None,
            Node::Literal(bytes) => Step::Literal(bytes.into_boxed_slice()),
            Node::Run(run) => Step::Run(run),
            Node::Save(slot) => Step::Save(slot),
            Node::Alt(branches) => {
                let branches = branches
                    .into_iter()
                    .map(|branch| Some(Step::sequence(branch)?.into_boxed_slice()))
                    .collect::<Option<Vec<_>>>()?;
                Step::Alt(Box::new(Alt::new(branches.into_boxed_slice())?))
            }
            Node::Group {
                body,
                min,
                max,
                greedy,
            } => {
                let body = Step::sequence(body)?;
                // Every repetition consumes, so none can repeat forever.
                let (Some(first), false) = starts(&body)? else {
                    return None;
                };
                Step::Group(Box::new(Group {
                    body: body.into_boxed_slice(),
                    min,
                    max,
                    greedy,
                    first,
                    tail: false,
                    snapshot: Box::new([]),
                    completion: Box::new([]),
                }))
            }
        })
    }

    fn sequence(nodes: Vec<Node>) -> Option<Vec<Step>> {
        nodes.into_iter().map(Step::from_node).collect()
    }

    /// The characters the step's first consumed character can be.
    fn first(&self) -> Option<Cow<'_, Class>> {
        match self {
            Step::Literal(bytes) => {
                let len = char_at(bytes, 0).map_or(1, |(_, len)| len);
                Class::from_literal(&bytes[..len]).map(Cow::Owned)
            }
            Step::Run(run) => Some(Cow::Borrowed(&run.class)),
            Step::Save(_) => None,
            Step::Alt(alt) => Some(Cow::Borrowed(&alt.first)),
            Step::Group(group) => Some(Cow::Borrowed(&group.first)),
        }
    }

    /// Whether the step must consume a character.
    fn consumes(&self) -> bool {
        match self {
            Step::Literal(_) | Step::Alt(_) => true,
            Step::Run(run) => run.min > 0,
            Step::Save(_) => false,
            Step::Group(group) => group.min > 0,
        }
    }
}

/// The characters `steps` can start with, if any, and whether they can match
/// nothing; `None` when those characters do not fit one class.
fn starts(steps: &[Step]) -> Option<(Option<Class>, bool)> {
    let mut first: Option<Class> = None;
    for step in steps {
        if let Some(class) = step.first() {
            first = Some(match first {
                Some(first) => first.union(&class)?,
                None => class.into_owned(),
            });
        }
        if step.consumes() {
            return Some((first, false));
        }
    }
    Some((first, true))
}

/// Every capture slot `steps` save, at any depth.
fn saves(steps: &[Step], slots: &mut Vec<usize>) {
    for step in steps {
        match step {
            Step::Save(slot) => slots.push(*slot),
            Step::Alt(alt) => {
                for branch in &*alt.branches {
                    saves(branch, slots);
                }
            }
            Step::Group(group) => saves(&group.body, slots),
            Step::Literal(_) | Step::Run(_) => {}
        }
    }
}

impl Alt {
    /// Alternatives the next character chooses between, or `None` if one can
    /// match nothing or two can start with the same character.
    fn new(branches: Box<[Box<[Step]>]>) -> Option<Alt> {
        let firsts = branches
            .iter()
            .map(|branch| match starts(branch)? {
                (Some(first), false) => Some(first),
                _ => None,
            })
            .collect::<Option<Vec<_>>>()?;
        let mut table = Box::new([NO_BRANCH; 256]);
        let mut by_byte = branches.len() < usize::from(NO_BRANCH);
        for (index, first) in firsts.iter().enumerate() {
            for b in bytes_in(first.first_bytes()) {
                by_byte &= table[usize::from(b)] == NO_BRANCH;
                table[usize::from(b)] = index as u8;
            }
        }
        // Branches sharing a first byte need their characters compared, which
        // only a few branches may.
        if !by_byte {
            if firsts.len() > 32 {
                return None;
            }
            for (index, first) in firsts.iter().enumerate() {
                if firsts[index + 1..]
                    .iter()
                    .any(|other| other.overlaps(first))
                {
                    return None;
                }
            }
        }
        let (head, rest) = firsts.split_first()?;
        let first = rest
            .iter()
            .try_fold(head.clone(), |first, class| first.union(class))?;
        Some(Alt {
            branches,
            firsts: firsts.into_boxed_slice(),
            by_byte: by_byte.then_some(table),
            first,
        })
    }

    /// The branch the character at `at` selects.
    #[inline]
    fn branch(&self, haystack: &[u8], at: usize, end: usize) -> Option<&[Step]> {
        let index = match &self.by_byte {
            Some(table) => match table[usize::from(*haystack[..end].get(at)?)] {
                NO_BRANCH => return None,
                index => usize::from(index),
            },
            None => self
                .firsts
                .iter()
                .position(|first| first.match_len(haystack, at, end).is_some())?,
        };
        Some(&self.branches[index])
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

/// Flatten `hir` into nodes, or `None` outside the regular shapes a program
/// can take.
fn flatten(
    hir: &Hir,
    nodes: &mut Vec<Node>,
    names: &mut Vec<(usize, Option<Box<str>>)>,
) -> Option<()> {
    match hir.kind() {
        HirKind::Empty => {}
        HirKind::Literal(hir::Literal(bytes)) => match nodes.last_mut() {
            Some(Node::Literal(previous)) => previous.extend_from_slice(bytes),
            _ => nodes.push(Node::Literal(bytes.to_vec())),
        },
        HirKind::Class(class) => {
            nodes.push(Node::Run(Run::new(Class::from_hir(class), 1, 1, true)));
        }
        HirKind::Look(hir_look) => nodes.push(Node::Look(look(*hir_look))),
        HirKind::Repetition(repetition) => {
            let max = repetition.max.map_or(usize::MAX, |max| max as usize);
            if max == 0 {
                return None;
            }
            let (min, greedy) = (repetition.min as usize, repetition.greedy);
            let class = match repetition.sub.kind() {
                HirKind::Class(class) => Some(Class::from_hir(class)),
                HirKind::Literal(hir::Literal(bytes)) => Class::from_literal(bytes),
                _ => None,
            };
            match class {
                Some(class) => nodes.push(Node::Run(Run::new(class, min, max, greedy))),
                None => {
                    let mut body = Vec::new();
                    flatten(&repetition.sub, &mut body, names)?;
                    nodes.push(Node::Group {
                        body,
                        min,
                        max,
                        greedy,
                    });
                }
            }
        }
        HirKind::Capture(capture) => {
            let index = capture.index as usize;
            names.push((index, capture.name.clone()));
            nodes.push(Node::Save(2 * index));
            flatten(&capture.sub, nodes, names)?;
            nodes.push(Node::Save(2 * index + 1));
        }
        HirKind::Concat(subs) => {
            for sub in subs {
                flatten(sub, nodes, names)?;
            }
        }
        HirKind::Alternation(subs) => {
            let mut branches = Vec::with_capacity(subs.len());
            for sub in subs {
                let mut branch = Vec::new();
                flatten(sub, &mut branch, names)?;
                branches.push(branch);
            }
            nodes.push(Node::Alt(branches));
        }
    }
    Some(())
}

/// What can come after a step, borrowing the classes of later steps.
#[derive(Clone)]
struct Follow<'a> {
    /// The characters that can come next, if any.
    next: Option<Cow<'a, Class>>,
    /// Whether the rest of the pattern can match nothing here: `None` where
    /// that depends on how often an enclosing group has repeated.
    end: Option<bool>,
    /// The slots the rest of the pattern sets when it matches nothing, or
    /// `None` for more than [`SNAPSHOT`].
    completion: Option<Vec<usize>>,
    /// The slots inside alternatives and groups in the rest of the pattern,
    /// or `None` for more than [`SNAPSHOT`].
    later: Option<Vec<usize>>,
}

impl<'a> Follow<'a> {
    /// Whether a character of `class` could come next.
    fn overlaps(&self, class: &Class) -> bool {
        self.next.as_ref().is_some_and(|next| next.overlaps(class))
    }

    /// `class` can come next as well; `None` if the characters do not fit
    /// one class.
    fn add(&mut self, class: Cow<'a, Class>) -> Option<()> {
        self.next = Some(match self.next.take() {
            Some(next) => Cow::Owned(next.union(&class)?),
            None => class,
        });
        Some(())
    }

    /// The same follow, owning its class.
    fn into_owned(self) -> Follow<'static> {
        Follow {
            next: self.next.map(|next| Cow::Owned(next.into_owned())),
            end: self.end,
            completion: self.completion,
            later: self.later,
        }
    }
}

/// Add `slots` to a bounded list of slots.
fn extend(list: &mut Option<Vec<usize>>, slots: impl IntoIterator<Item = usize>) {
    if let Some(vec) = list {
        vec.extend(slots);
        if vec.len() > SNAPSHOT {
            *list = None;
        }
    }
}

/// Admit `steps`, followed by `outer`, where the next character decides every
/// choice: mark lazy runs that stop at their minimum and groups that record
/// where they could stop. Returns what follows the step at `want`.
fn admit<'a>(
    steps: &'a mut [Step],
    outer: &Follow<'a>,
    trailing: &[Look],
    want: Option<usize>,
) -> Option<Option<Follow<'static>>> {
    let mut wanted = None;
    let mut follow = outer.clone();
    let mut rest = steps;
    while let Some((step, before)) = rest.split_last_mut() {
        if want == Some(before.len()) {
            wanted = Some(follow.clone().into_owned());
        }
        admit_step(step, &follow, trailing)?;
        // Admitted, the step's classes can be borrowed by the follows of
        // the steps before it.
        let step: &'a Step = step;
        rest = before;
        match step {
            Step::Save(slot) => {
                extend(&mut follow.completion, [*slot]);
                continue;
            }
            Step::Alt(_) | Step::Group(_) => {
                let mut nested = Vec::new();
                saves(core::slice::from_ref(step), &mut nested);
                extend(&mut follow.later, nested);
            }
            Step::Literal(_) | Step::Run(_) => {}
        }
        let first = step.first()?;
        if step.consumes() {
            follow = Follow {
                next: Some(first),
                end: Some(false),
                completion: Some(Vec::new()),
                later: Some(Vec::new()),
            };
        } else {
            follow.add(first)?;
        }
    }
    Some(wanted)
}

fn admit_step(step: &mut Step, follow: &Follow<'_>, trailing: &[Look]) -> Option<()> {
    match step {
        Step::Literal(_) | Step::Save(_) => {}
        Step::Run(run) => {
            if run.min != run.max {
                if follow.overlaps(&run.class) {
                    return None;
                }
                match follow.end {
                    Some(true) => {
                        if !trailing.is_empty() && !trailing.iter().any(|&look| ends_run(look, run))
                        {
                            return None;
                        }
                        run.shortest = run.lazy && trailing.is_empty();
                    }
                    None if run.lazy || !trailing.is_empty() => return None,
                    _ => {}
                }
            }
        }
        Step::Alt(alt) => {
            for branch in alt.branches.iter_mut() {
                admit(branch, follow, trailing, None)?;
            }
        }
        Step::Group(group) => {
            let choice = group.min != group.max;
            if choice && follow.overlaps(&group.first) {
                return None;
            }
            let mut inner = follow.clone();
            if group.max > 1 {
                inner.add(Cow::Borrowed(&group.first))?;
            }
            let mut nested = Vec::new();
            saves(&group.body, &mut nested);
            extend(&mut inner.later, nested);
            inner.end = match follow.end {
                Some(true) if group.min <= 1 => Some(true),
                Some(false) => Some(false),
                _ => None,
            };
            let later = inner.later.clone();
            admit(&mut group.body, &inner, trailing, None)?;
            if choice {
                match follow.end {
                    Some(true) => {
                        // A lazy group would stop wherever the assertions hold.
                        if !group.greedy && !trailing.is_empty() {
                            return None;
                        }
                        group.tail = true;
                        group.snapshot = later?.into_boxed_slice();
                        group.completion = follow.completion.clone()?.into_boxed_slice();
                    }
                    None => return None,
                    Some(false) => {}
                }
            }
        }
    }
    Some(())
}

/// How a sequence changes an attempt's exposure: the bytes it has read where
/// another attempt could start, since it started or last recorded a match.
/// From exposure `x` it leaves at most `max(x + add, floor)`, and it reads at
/// most [`RESCAN_BOUND`] such bytes before each point where it can fail as
/// long as `x <= limit`. `usize::MAX` stands for unbounded.
#[derive(Clone, Copy)]
struct Exposure {
    add: usize,
    floor: usize,
    limit: usize,
}

impl Exposure {
    const NONE: Exposure = Exposure {
        add: 0,
        floor: 0,
        limit: usize::MAX,
    };

    /// Reading `add` such bytes, with a point where the attempt can fail
    /// after `fails_after` of them.
    fn read(add: usize, fails_after: Option<usize>) -> Option<Exposure> {
        let limit = match fails_after {
            Some(read) => RESCAN_BOUND.checked_sub(read)?,
            None => usize::MAX,
        };
        Some(Exposure {
            add,
            floor: 0,
            limit,
        })
    }

    /// `self`, then `next`.
    fn then(self, next: Exposure) -> Option<Exposure> {
        let limit = if next.limit == usize::MAX {
            self.limit
        } else if self.floor > next.limit {
            return None;
        } else {
            self.limit.min(next.limit.checked_sub(self.add)?)
        };
        Some(Exposure {
            add: self.add.saturating_add(next.add),
            floor: self.floor.saturating_add(next.add).max(next.floor),
            limit,
        })
    }

    /// One of two alternatives.
    fn or(self, other: Exposure) -> Exposure {
        Exposure {
            add: self.add.max(other.add),
            floor: self.floor.max(other.floor),
            limit: self.limit.min(other.limit),
        }
    }

    /// `count` repetitions of `self`.
    fn times(self, count: usize) -> Option<Exposure> {
        let Some(more) = count.checked_sub(1) else {
            return Some(Exposure::NONE);
        };
        let limit = if self.limit == usize::MAX {
            usize::MAX
        } else {
            if more > 0 && self.floor.saturating_add(self.add.saturating_mul(more - 1)) > self.limit
            {
                return None;
            }
            self.limit.checked_sub(self.add.saturating_mul(more))?
        };
        Some(Exposure {
            add: self.add.saturating_mul(count),
            floor: self.floor.saturating_add(self.add.saturating_mul(more)),
            limit,
        })
    }

    /// Any number of repetitions of `self`.
    fn repeated(self) -> Option<Exposure> {
        if self.add == 0 {
            // Every repetition after the first leaves the same exposure.
            return self.times(2);
        }
        // Unbounded: the next point where the attempt can fail, which a
        // repetition that records no match always has after it, rejects the
        // program.
        Some(Exposure {
            add: usize::MAX,
            floor: usize::MAX,
            limit: usize::MAX,
        })
    }
}

/// What bounds rescans in a program.
struct Rescan<'a> {
    /// The characters a match can start with.
    first: &'a Class,
    /// Assertions follow the program, so a group records a match only where
    /// they hold.
    trailing: bool,
    /// The top-level index of an unbounded first run a failure skips.
    skip: Option<usize>,
    /// The top-level index of a first literal the candidates confirm.
    confirmed: Option<usize>,
    /// The characters a lookbehind needs right before every match start: no
    /// attempt can start after a character outside them.
    before: Option<Class>,
}

impl Rescan<'_> {
    /// The exposure of `steps`, at the top level when `top`, or `None` where
    /// an attempt could read more than [`RESCAN_BOUND`] bytes where another
    /// could start before failing.
    fn sequence(&self, steps: &[Step], top: bool) -> Option<Exposure> {
        steps
            .iter()
            .enumerate()
            .try_fold(Exposure::NONE, |exposure, (index, step)| {
                exposure.then(self.step(step, top.then_some(index))?)
            })
    }

    fn step(&self, step: &Step, index: Option<usize>) -> Option<Exposure> {
        match step {
            Step::Save(_) => Some(Exposure::NONE),
            Step::Literal(bytes) => {
                let held = (0..bytes.len())
                    .any(|at| self.first.match_len(bytes, at, bytes.len()).is_some());
                let held = if held { bytes.len() } else { 0 };
                let fails = index.is_none() || index != self.confirmed;
                Exposure::read(held, fails.then_some(held))
            }
            Step::Run(run) => {
                // Inside a run of characters a lookbehind cannot end with,
                // attempts fail at once.
                let held = run.class.overlaps(self.first)
                    && self
                        .before
                        .as_ref()
                        .is_none_or(|before| before.overlaps(&run.class));
                let width = if held { run.class.max_len() } else { 0 };
                let fails_after = (run.min > 0).then(|| run.min.saturating_mul(width));
                let add = if run.max != usize::MAX {
                    run.max.saturating_mul(width)
                } else if !held || index.is_some() && index == self.skip {
                    0
                } else {
                    usize::MAX
                };
                Exposure::read(add, fails_after)
            }
            Step::Alt(alt) => {
                let mut branches: Option<Exposure> = None;
                for branch in &*alt.branches {
                    let exposure = self.sequence(branch, false)?;
                    branches = Some(branches.map_or(exposure, |other| other.or(exposure)));
                }
                // Choosing a branch can fail on the first character.
                Exposure::read(0, Some(0))?.then(branches?)
            }
            Step::Group(group) => {
                let body = self.sequence(&group.body, false)?;
                let further = if group.max == group.min {
                    Exposure::NONE
                } else if group.tail && !self.trailing {
                    // Each further repetition starts at a recorded match.
                    Exposure {
                        add: 0,
                        floor: body.add.max(body.floor),
                        limit: usize::MAX,
                    }
                } else if group.max == usize::MAX {
                    body.repeated()?
                } else {
                    body.times(group.max - group.min)?
                };
                body.times(group.min)?.then(further)
            }
        }
    }
}

/// For a program starting with an unbounded run that a literal follows,
/// whose first character the run cannot take, that literal and the run's
/// class. Every match then starts with the maximal run ending where the
/// literal starts, so its occurrences give the candidates. `unasserted`: no
/// assertion is tested where a match starts, which could fail at the run's
/// start but hold later in it.
fn suffix(steps: &[Step], first: usize, unasserted: bool) -> Option<(Box<[u8]>, Class)> {
    let Step::Run(run) = &steps[first] else {
        return None;
    };
    if !unasserted || run.max != usize::MAX || run.min == 0 {
        return None;
    }
    match steps[first + 1..]
        .iter()
        .find(|step| !matches!(step, Step::Save(_)))?
    {
        step @ Step::Literal(bytes) if !step.first()?.overlaps(&run.class) => {
            Some((bytes.clone(), run.class.clone()))
        }
        _ => None,
    }
}

/// The prefixes of `hir` regex-automata's meta engine extracts for a
/// leftmost-first prefilter, optimized by preference as it does.
/// Whether `hir` can be a set of literals its prefixes find exactly: only a
/// pattern of bounded length that cannot match nothing, without captures or
/// assertions, can be.
fn may_be_exact(hir: &Hir) -> bool {
    let properties = hir.properties();
    properties.maximum_len().is_some()
        && properties.minimum_len().is_some_and(|len| len > 0)
        && properties.explicit_captures_len() == 0
        && properties.look_set().is_empty()
}

fn prefixes(hir: &Hir) -> literal::Seq {
    let mut extractor = literal::Extractor::new();
    extractor.kind(literal::ExtractKind::Prefix);
    let mut prefixes = extractor.extract(hir);
    prefixes.optimize_for_prefix_by_preference();
    prefixes
}

impl Plan {
    /// The plan for `hir`, or `None` when its matches do not follow one
    /// deterministic path the next character decides. With `prefilter`, a
    /// plan starting with alternatives, a group or a short fixed sequence of
    /// classes looks for its prefixes with a regex-automata prefilter, as the
    /// meta engine would.
    pub(crate) fn new(hir: &Hir, prefilter: bool) -> Option<Plan> {
        Plan::with_edges(hir, prefilter, &Edges::default())
    }

    /// [`Plan::new`] for `hir` between look-around `edges`. A lookahead is
    /// admitted where the end of the match is already decided: its first
    /// characters follow the last step as a step would, and a negative one
    /// only follows a step of fixed length.
    pub(crate) fn with_edges(hir: &Hir, prefilter: bool, edges: &Edges) -> Option<Plan> {
        // Unicode word boundaries without their data tables: the engines
        // refuse to build, and the look-around matcher would panic.
        if hir.properties().look_set().contains_word_unicode() {
            LookSet::singleton(Look::WordUnicode).available().ok()?;
        }
        // Prefixes that are not exactly the matches can still find candidates.
        let mut extracted = None;
        if prefilter && edges.behind.is_empty() && edges.ahead.is_empty() && may_be_exact(hir) {
            let prefixes = match hir.kind() {
                HirKind::Literal(hir::Literal(bytes)) => literal::Seq::new([bytes]),
                _ => prefixes(hir),
            };
            if let Some(plan) = Plan::literals(hir, &prefixes) {
                return Some(plan);
            }
            extracted = Some(prefixes);
        }
        let mut nodes = Vec::new();
        let mut names = Vec::new();
        flatten(hir, &mut nodes, &mut names)?;
        let consuming = |node: &Node| !matches!(node, Node::Look(_) | Node::Save(_));
        let first_node = nodes.iter().position(consuming)?;
        let last_node = nodes.iter().rposition(consuming)?;
        let mut leading = Vec::new();
        let mut trailing = Vec::new();
        let mut steps = Vec::new();
        for (index, node) in nodes.into_iter().enumerate() {
            match node {
                Node::Look(look) if index < first_node => leading.push(look),
                Node::Look(look) if index > last_node => trailing.push(look),
                node => steps.push(Step::from_node(node)?),
            }
        }
        let start_only = leading.contains(&Look::Start);
        // A reverse search from the end finds these matches sooner.
        if trailing.contains(&Look::End) && !start_only {
            return None;
        }
        let first = steps
            .iter()
            .position(|step| !matches!(step, Step::Save(_)))?;
        let (first_class, nullable) = starts(&steps)?;
        let first_class = first_class?;
        let edged = !edges.behind.is_empty() || !edges.ahead.is_empty();
        // Assertions would decide where a match that can be empty starts.
        if nullable && (!leading.is_empty() || !trailing.is_empty() || edged) {
            return None;
        }
        let mut outer = Follow {
            next: None,
            end: Some(true),
            completion: Some(Vec::new()),
            later: Some(Vec::new()),
        };
        for edge in edges.ahead.iter().filter(|edge| !edge.negated) {
            // An empty alternative always holds.
            if edge
                .alternatives
                .iter()
                .any(|alternative| alternative.is_empty())
            {
                continue;
            }
            outer.end = Some(false);
            for alternative in &*edge.alternatives {
                outer.add(Cow::Borrowed(&alternative[0]))?;
            }
        }
        let first_follow = admit(&mut steps, &outer, &trailing, Some(first))??;
        if edges.ahead.iter().any(|edge| edge.negated) {
            let last = steps
                .iter()
                .rposition(|step| !matches!(step, Step::Save(_)))?;
            match &steps[last] {
                Step::Literal(_) => {}
                Step::Run(run) if run.min == run.max => {}
                _ => return None,
            }
        }
        let checks_end = !trailing.is_empty() || !edges.ahead.is_empty();
        let decisive = if !checks_end {
            steps
                .iter()
                .rposition(Step::consumes)
                .map_or(0, |index| index + 1)
        } else {
            steps.len()
        };
        let required = steps[first + 1..]
            .iter()
            .filter_map(|step| match step {
                Step::Literal(bytes) => Some(bytes),
                _ => None,
            })
            // The last of the longest, like the suffix a reverse search uses.
            .max_by_key(|bytes| bytes.len())
            .map(|bytes| Box::new(memchr::memmem::Finder::new(bytes).into_owned()));
        // A literal after steps of fixed byte length sits at a fixed offset
        // from every match start, so its occurrences give the candidates.
        let mut offset = 0;
        let mut inner: Option<(usize, &[u8])> = None;
        for (index, step) in steps.iter().enumerate().skip(first) {
            match step {
                Step::Literal(bytes) => {
                    if index != first && inner.is_none_or(|(_, best)| bytes.len() > best.len()) {
                        inner = Some((offset, bytes));
                    }
                    offset += bytes.len();
                }
                Step::Run(run) if run.min == run.max && run.class.max_len() == 1 => {
                    offset += run.min;
                }
                _ => break,
            }
        }
        let candidates = match (&steps[first], inner) {
            (Step::Run(_), Some((offset, bytes))) => Candidates::Offset(
                Box::new(memchr::memmem::Finder::new(bytes).into_owned()),
                offset,
            ),
            (Step::Literal(bytes), _) => {
                Candidates::Literal(Box::new(memchr::memmem::Finder::new(bytes).into_owned()))
            }
            (step, _) => {
                // Prefixes of a run of variable length are no better than
                // its class; other steps have a short set of prefixes.
                let short = match step {
                    Step::Run(run) => run.min == run.max,
                    _ => true,
                };
                let prefilter = (prefilter && short && !nullable)
                    .then(|| extracted.unwrap_or_else(|| prefixes(hir)))
                    .as_ref()
                    .and_then(literal::Seq::literals)
                    .and_then(|literals| Prefilter::new(MatchKind::LeftmostFirst, literals));
                let first_bytes = first_class.first_bytes();
                let stops = Stops::of(&mut bytes_in(first_bytes));
                match (prefilter, stops) {
                    (Some(prefilter), _) if prefilter.is_fast() => Candidates::Prefilter(prefilter),
                    (_, Some(stops)) => Candidates::Bytes(stops),
                    (prefilter, None) => match (ByteRanges::of(&first_bytes), prefilter) {
                        (Some(ranges), _) => Candidates::Ranges(ranges, first_class.clone()),
                        // Even with prefixes too short for regex-automata to
                        // call it fast, as two-byte keywords, a literal
                        // search beats testing every byte. Fixed runs keep
                        // their class scan, as before.
                        (None, Some(prefilter)) if !matches!(step, Step::Run(_)) => {
                            Candidates::Prefilter(prefilter)
                        }
                        (None, _) => Candidates::Class(first_class.clone()),
                    },
                }
            }
        };
        let skip_first_run = matches!(&steps[first], Step::Run(run) if run.max == usize::MAX);
        let candidates = match candidates {
            candidates @ (Candidates::Literal(_) | Candidates::Offset(..)) => candidates,
            Candidates::Prefilter(prefilter) if prefilter.is_fast() => {
                Candidates::Prefilter(prefilter)
            }
            weaker => match (
                edges.behind.iter().find_map(Edge::literals),
                suffix(&steps, first, leading.is_empty() && edges.behind.is_empty()),
            ) {
                // The literals of a lookbehind end where matches start.
                (Some((literals, len)), _) => Prefilter::new(MatchKind::LeftmostFirst, &literals)
                    .map_or(weaker, |prefilter| Candidates::Behind(prefilter, len)),
                (None, Some((literal, class))) => Candidates::Suffix(
                    Box::new(memchr::memmem::Finder::new(&literal).into_owned()),
                    class,
                ),
                (None, None) => weaker,
            },
        };
        let look_skip = skip_first_run
            && leading.is_empty()
            && !edges.behind.is_empty()
            && match &steps[first] {
                Step::Run(run) => edges.behind.iter().all(|edge| {
                    !edge.negated
                        && edge.alternatives.iter().all(|alternative| {
                            alternative
                                .last()
                                .is_some_and(|class| !class.overlaps(&run.class))
                        })
                }),
                _ => false,
            };
        let rescan = Rescan {
            first: &first_class,
            trailing: checks_end,
            skip: skip_first_run.then_some(first),
            confirmed: matches!(candidates, Candidates::Literal(_)).then_some(first),
            before: edges.behind.iter().find_map(Edge::last_classes),
        };
        // A search tries one start when only its start can begin a match.
        if !start_only {
            let exposure = rescan.sequence(&steps, true)?;
            if checks_end {
                // The assertions can fail where the last step ends.
                exposure.then(Exposure::read(0, Some(0))?)?;
            }
        }
        let window = !edged
            && !matches!(candidates, Candidates::Offset(..))
            && match &steps[first] {
                // Retrying shorter runs costs little more than a window.
                Step::Run(run)
                    if run.max != usize::MAX
                        && run.max > 2
                        && run.min > 0
                        && (run.min == run.max || !run.shortest) =>
                {
                    let can_end = first_follow.end == Some(true);
                    (!can_end || !trailing.is_empty())
                        && !first_follow.overlaps(&run.class)
                        && (!can_end || trailing.iter().any(|&look| ends_run(look, run)))
                }
                _ => false,
            };
        let mut conditional = Vec::new();
        let mut branching = false;
        for step in &steps {
            if matches!(step, Step::Alt(_) | Step::Group(_)) {
                branching = true;
                saves(core::slice::from_ref(step), &mut conditional);
            }
        }
        let nfa_units = nfa_bound(hir);
        let mut group_names = Vec::new();
        group_names.resize(hir.properties().explicit_captures_len() + 1, None);
        for (index, name) in names {
            *group_names.get_mut(index)? = name;
        }
        let group_info = GroupInfo::new([group_names]).ok()?;
        let checks_start = !leading.is_empty() || !edges.behind.is_empty();
        let single_run = !checks_start
            && !checks_end
            && !start_only
            && matches!(&*steps, [Step::Run(run)] if run.max == usize::MAX && run.min > 0);
        Some(Plan {
            steps: steps.into_boxed_slice(),
            leading: leading.into_boxed_slice(),
            trailing: trailing.into_boxed_slice(),
            checks_start,
            checks_end,
            edges: edged.then(|| Box::new(edges.clone())),
            candidates,
            required,
            start_only,
            whole: if nullable {
                Whole::Empty
            } else if single_run {
                Whole::Run
            } else {
                Whole::Attempts
            },
            skip_first_run,
            look_skip,
            first,
            window,
            decisive,
            conditional: conditional.into_boxed_slice(),
            branching,
            look_matcher: LookMatcher::new(),
            group_info,
            nfa_units,
        })
    }

    /// The plan for a pattern whose `prefixes` are exactly its matches, as
    /// the meta engine's literal strategy finds them: a finite set of
    /// literals, without captures or assertions (see [`may_be_exact`]), that
    /// its leftmost-first prefilter reports in the pattern's order of
    /// preference. It needs no program.
    fn literals(hir: &Hir, prefixes: &literal::Seq) -> Option<Plan> {
        if !prefixes.is_exact() {
            return None;
        }
        let prefilter = Prefilter::new(MatchKind::LeftmostFirst, prefixes.literals()?)?;
        Some(Plan {
            steps: Box::new([]),
            leading: Box::new([]),
            trailing: Box::new([]),
            edges: None,
            checks_start: false,
            checks_end: false,
            candidates: Candidates::Prefilter(prefilter),
            required: None,
            start_only: false,
            whole: Whole::Literals,
            skip_first_run: false,
            first: 0,
            look_skip: false,
            window: false,
            decisive: 0,
            conditional: Box::new([]),
            branching: false,
            look_matcher: LookMatcher::new(),
            group_info: GroupInfo::new([[None::<&str>]]).ok()?,
            nfa_units: nfa_bound(hir),
        })
    }

    /// [`nfa_bound`] of the pattern, compared with [`NFA_UNIT_BUDGET`].
    pub(crate) fn nfa_units(&self) -> usize {
        self.nfa_units
    }

    /// Whether the program has alternatives or groups, which can read past
    /// the end of the match they report.
    pub(crate) fn branching(&self) -> bool {
        self.branching
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
    fn next_candidate(&self, input: &PlanInput<'_>, from: usize) -> Option<usize> {
        let (haystack, end) = (input.haystack, input.end);
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
            Candidates::Behind(prefilter, len) => {
                // A literal ending at or after `from`, perhaps before the
                // search's start; all have the same length, so their ends
                // come in order.
                let start = from.saturating_sub(*len);
                return prefilter
                    .find(haystack, Span { start, end })
                    .map(|found| found.start + len);
            }
            Candidates::Suffix(finder, class) => {
                // Decoding backwards needs valid UTF-8 to agree with a
                // forward run.
                if !input.utf8 && matches!(class, Class::Chars(_)) {
                    return Plan::find_class(class, haystack, from, end);
                }
                let mut at = from;
                while let Some(offset) = finder.find(&haystack[at..end]) {
                    let suffix = at + offset;
                    let mut start = suffix;
                    while let Some(len) = class.len_before(haystack, start)
                        && start - len >= from
                    {
                        start -= len;
                    }
                    if start < suffix {
                        return Some(start);
                    }
                    at = suffix + 1;
                }
                return None;
            }
        };
        found.map(|offset| from + offset)
    }

    /// The first position in `from..end` holding a member of `class`, for
    /// candidates that cannot use their faster search on this input.
    #[inline(never)]
    fn find_class(class: &Class, haystack: &[u8], from: usize, end: usize) -> Option<usize> {
        match class {
            Class::Bytes(set) => haystack[from..end]
                .iter()
                .position(|&b| has_byte(set, b))
                .map(|offset| from + offset),
            Class::Chars(set) => {
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
                None
            }
        }
    }

    /// Where `run` ends from `at`, or `None` if it has fewer than `run.min`
    /// members: after `run.min` members when it is shortest or `decided`, else
    /// after as many as it can take. A single character, the commonest run
    /// (a class or a case-folded letter), is tested directly.
    #[inline(always)]
    fn run_end(run: &Run, input: &PlanInput<'_>, at: usize, decided: bool) -> Option<usize> {
        if run.max == 1 && run.min == 1 {
            return run
                .class
                .match_len(input.haystack, at, input.end)
                .map(|len| at + len);
        }
        let (stop, count) = if run.shortest || decided {
            Plan::take(run, input.haystack, at, input.end, run.min)
        } else {
            Plan::scan(run, input.haystack, at, input.end, input.utf8)
        };
        (count >= run.min).then_some(stop)
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
        Plan::take(run, haystack, at, end, run.max)
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
                    return self.look_decoded(look, haystack, at);
                }
                (word(before), word(after))
            }
            Look::StartCRLF | Look::EndCRLF => return self.look_decoded(look, haystack, at),
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

    /// `look` at `at` as regex-automata decides it, decoding the characters
    /// around it: one copy of its tests, rather than one wherever
    /// [`Plan::look_at`] is inlined.
    #[cold]
    #[inline(never)]
    fn look_decoded(&self, look: Look, haystack: &[u8], at: usize) -> bool {
        self.look_matcher.matches(look, haystack, at)
    }

    /// Whether every trailing assertion holds at `at`.
    #[inline]
    fn trailing_hold(&self, haystack: &[u8], at: usize) -> bool {
        !self.checks_end
            || self
                .trailing
                .iter()
                .all(|&look| self.look_at(look, haystack, at))
                && self
                    .edges
                    .as_ref()
                    .is_none_or(|edges| Plan::ahead_holds(edges, haystack, at))
    }

    /// Whether every leading assertion holds at `at`.
    #[inline]
    fn start_holds(&self, haystack: &[u8], at: usize) -> bool {
        self.leading
            .iter()
            .all(|&look| self.look_at(look, haystack, at))
            && self
                .edges
                .as_ref()
                .is_none_or(|edges| Plan::behind_holds(edges, haystack, at))
    }

    /// The lookaheads, out of line: ordinary assertions stay inline.
    #[inline(never)]
    fn ahead_holds(edges: &Edges, haystack: &[u8], at: usize) -> bool {
        edges.ahead.iter().all(|edge| edge.ahead(haystack, at))
    }

    /// The lookbehinds, out of line: ordinary assertions stay inline.
    #[inline(never)]
    fn behind_holds(edges: &Edges, haystack: &[u8], at: usize) -> bool {
        edges.behind.iter().all(|edge| edge.behind(haystack, at))
    }

    /// Match `steps` from `state.pos`. Steps without steps inside them are
    /// matched in the loop itself: a call for each, as for a case-folded
    /// letter, would cost more than its test.
    #[inline]
    fn sequence<'p, S: Slots + ?Sized>(
        &'p self,
        steps: &'p [Step],
        input: &PlanInput<'_>,
        state: &mut State<'p, '_, S>,
        earliest: bool,
    ) -> Result<(), Stop> {
        let (haystack, end) = (input.haystack, input.end);
        for step in steps {
            match step {
                Step::Literal(bytes) => {
                    if !literal_at(haystack, state.pos, end, bytes) {
                        return Err(Stop::Fail);
                    }
                    state.pos += bytes.len();
                }
                Step::Run(run) => {
                    state.pos = Plan::run_end(run, input, state.pos, false).ok_or(Stop::Fail)?;
                }
                Step::Save(slot) => {
                    if let Some(slots) = state.slots.as_deref_mut() {
                        slots.set(*slot, state.pos);
                    }
                }
                Step::Alt(alt) => {
                    let branch = alt.branch(haystack, state.pos, end).ok_or(Stop::Fail)?;
                    self.sequence(branch, input, state, earliest)?;
                }
                Step::Group(group) => self.group(group, input, state, earliest, false)?,
            }
        }
        Ok(())
    }

    /// Repeat `group` as the next character decides, at most its minimum
    /// number of times when `decided`. Where the rest of the pattern could
    /// match nothing, a greedy group records the position before repeating
    /// and a lazy one stops.
    fn group<'p, S: Slots + ?Sized>(
        &'p self,
        group: &'p Group,
        input: &PlanInput<'_>,
        state: &mut State<'p, '_, S>,
        earliest: bool,
        decided: bool,
    ) -> Result<(), Stop> {
        let (haystack, end) = (input.haystack, input.end);
        let limit = if decided { group.min } else { group.max };
        let mut count = 0;
        while count < limit {
            if count >= group.min {
                if group.tail {
                    if !group.greedy {
                        break;
                    }
                    let ends = self.trailing_hold(haystack, state.pos);
                    if ends && earliest {
                        return Err(Stop::Found(state.pos));
                    }
                    if group.first.match_len(haystack, state.pos, end).is_none() {
                        break;
                    }
                    if ends {
                        state.record(group);
                    }
                } else if group.first.match_len(haystack, state.pos, end).is_none() {
                    break;
                }
            }
            self.sequence(&group.body, input, state, earliest)?;
            count += 1;
        }
        Ok(())
    }

    /// Match the program anchored at `start`, recording capture slots when
    /// given. A membership test only runs the decisive steps. On failure,
    /// [`LOOK_MISS`] if a leading assertion failed, else where the first run
    /// ended, or `start` when the first step is not a run.
    #[inline]
    fn attempt<S: Slots + ?Sized>(
        &self,
        input: &PlanInput<'_>,
        start: usize,
        earliest: bool,
        slots: Option<&mut S>,
    ) -> Result<usize, usize> {
        if self.branching {
            self.attempt_program::<true, S>(input, start, earliest, false, slots)
        } else {
            self.attempt_program::<false, S>(input, start, earliest, false, slots)
        }
    }

    /// [`Plan::attempt`], compiled without alternatives and groups, and so
    /// without recorded matches, unless `BRANCHING`. `confirmed`: the
    /// candidate finder already found the first literal at `start`.
    #[inline]
    fn attempt_program<const BRANCHING: bool, S: Slots + ?Sized>(
        &self,
        input: &PlanInput<'_>,
        start: usize,
        earliest: bool,
        confirmed: bool,
        mut slots: Option<&mut S>,
    ) -> Result<usize, usize> {
        let (haystack, end) = (input.haystack, input.end);
        if self.checks_start && !self.start_holds(haystack, start) {
            return Err(LOOK_MISS);
        }
        if let Some(slots) = slots.as_deref_mut() {
            for &slot in &*self.conditional {
                slots.clear(slot);
            }
        }
        let steps = if earliest {
            &self.steps[..self.decisive]
        } else {
            &self.steps[..]
        };
        let mut state = State {
            pos: start,
            slots,
            accept: None,
            saved: None,
        };
        let mut first_run_end = start;
        // Literals, runs and saves, which linear plans consist of, are
        // matched here; alternatives and groups recurse.
        let outcome = 'program: {
            for (index, step) in steps.iter().enumerate() {
                match step {
                    Step::Literal(bytes) => {
                        if !(confirmed && index == self.first)
                            && !literal_at(haystack, state.pos, end, bytes)
                        {
                            break 'program Err(Stop::Fail);
                        }
                        state.pos += bytes.len();
                    }
                    Step::Run(run) => {
                        let decided =
                            earliest && index + 1 == self.decisive && self.trailing.is_empty();
                        let Some(stop) = Plan::run_end(run, input, state.pos, decided) else {
                            break 'program Err(Stop::Fail);
                        };
                        if index == self.first {
                            first_run_end = stop;
                        }
                        state.pos = stop;
                    }
                    Step::Save(slot) => {
                        if let Some(slots) = state.slots.as_deref_mut() {
                            slots.set(*slot, state.pos);
                        }
                    }
                    Step::Group(group) if BRANCHING => {
                        let decided =
                            earliest && index + 1 == self.decisive && self.trailing.is_empty();
                        if let Err(stop) = self.group(group, input, &mut state, earliest, decided) {
                            break 'program Err(stop);
                        }
                    }
                    Step::Alt(alt) if BRANCHING => {
                        let Some(branch) = alt.branch(haystack, state.pos, end) else {
                            break 'program Err(Stop::Fail);
                        };
                        if let Err(stop) = self.sequence(branch, input, &mut state, earliest) {
                            break 'program Err(stop);
                        }
                    }
                    Step::Group(_) | Step::Alt(_) => {
                        unreachable!("a plan without alternatives or groups has none")
                    }
                }
            }
            Ok(())
        };
        #[cfg(all(test, feature = "std"))]
        tests::COVERED.with(|covered| covered.set(covered.get() + state.pos - start + 1));
        match outcome {
            Ok(()) if self.trailing_hold(haystack, state.pos) => Ok(state.pos),
            Err(Stop::Found(at)) if BRANCHING => Ok(at),
            _ if BRANCHING => state.fall_back().ok_or(first_run_end),
            _ => Err(first_run_end),
        }
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
        let (run_end, total) = Plan::take(run, haystack, candidate, end, usize::MAX);
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
        slots: Option<&mut S>,
    ) -> Option<(usize, usize)> {
        let start = input.start;
        match self.whole {
            Whole::Attempts => {}
            Whole::Run => return self.search_run(input, earliest),
            Whole::Empty | Whole::Literals => return self.search_whole(input, earliest, slots),
        }
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
        if self.branching {
            self.search_candidates::<true, S>(input, earliest, slots)
        } else {
            self.search_candidates::<false, S>(input, earliest, slots)
        }
    }

    /// Attempts at each candidate start in turn, compiled per kind of program
    /// as [`Plan::attempt_program`] is.
    #[inline(always)]
    fn search_candidates<const BRANCHING: bool, S: Slots + ?Sized>(
        &self,
        input: &PlanInput<'_>,
        earliest: bool,
        mut slots: Option<&mut S>,
    ) -> Option<(usize, usize)> {
        // A literal finder only reports where the first literal occurs.
        let confirmed = matches!(self.candidates, Candidates::Literal(_));
        let mut from = input.start;
        loop {
            let candidate = self.next_candidate(input, from)?;
            let attempt = self.attempt_program::<BRANCHING, S>(
                input,
                candidate,
                earliest,
                confirmed,
                slots.as_deref_mut(),
            );
            let resume = match attempt {
                Ok(match_end) => return Some((candidate, match_end)),
                Err(failure) => self.resume_after(input, candidate, failure),
            };
            if resume >= input.end {
                return None;
            }
            from = resume;
        }
    }

    /// Where to look for the next candidate after an attempt at `candidate`
    /// failed with `failure` (see [`Plan::attempt`]).
    #[inline]
    fn resume_after(&self, input: &PlanInput<'_>, candidate: usize, failure: usize) -> usize {
        if failure != LOOK_MISS {
            // Every later start inside an unbounded first run ends it at the
            // same place, and so fails the same way.
            if self.skip_first_run {
                return failure.max(candidate + 1);
            }
        } else if self.look_skip {
            // A lookbehind failing at the start of the first run fails
            // throughout it.
            return self.past_first_run(input, candidate);
        }
        candidate + 1
    }

    /// Where the maximal run of the first step's class from `candidate` ends,
    /// at least a byte later: where to look next when no start inside that
    /// run can match. Out of line, so that the run scan does not enlarge
    /// every search loop.
    #[cold]
    #[inline(never)]
    fn past_first_run(&self, input: &PlanInput<'_>, candidate: usize) -> usize {
        let Step::Run(run) = &self.steps[self.first] else {
            unreachable!("only a plan starting with a run skips past it");
        };
        let (run_end, _) = Plan::take(run, input.haystack, candidate, input.end, usize::MAX);
        run_end.max(candidate + 1)
    }

    /// `search` for a program that is one unbounded run: no attempt, just the
    /// run from each candidate. A membership test needs only its minimum.
    #[inline]
    fn search_run(&self, input: &PlanInput<'_>, earliest: bool) -> Option<(usize, usize)> {
        let Step::Run(run) = &self.steps[0] else {
            unreachable!("a plan of one run has a run");
        };
        if input.anchored {
            let end = Plan::run_end(run, input, input.start, earliest)?;
            return Some((input.start, end));
        }
        let mut from = input.start;
        loop {
            let candidate = self.next_candidate(input, from)?;
            if let Some(end) = Plan::run_end(run, input, candidate, earliest) {
                return Some((candidate, end));
            }
            // Shorter than its minimum, or not a member after all: so is
            // every later start inside the same run.
            from = self.past_first_run(input, candidate);
            if from >= input.end {
                return None;
            }
        }
    }

    /// `search` for a plan whose candidates are its matches, or whose
    /// pattern can match nothing.
    #[inline(never)]
    fn search_whole<S: Slots + ?Sized>(
        &self,
        input: &PlanInput<'_>,
        earliest: bool,
        slots: Option<&mut S>,
    ) -> Option<(usize, usize)> {
        if self.whole == Whole::Empty {
            return self.search_empty(input, earliest, slots);
        }
        let Candidates::Prefilter(prefilter) = &self.candidates else {
            unreachable!("a plan of literals searches with its prefilter");
        };
        let span = Span {
            start: input.start,
            end: input.end,
        };
        let found = if input.anchored {
            // A single-byte prefilter tests the byte at the start even
            // when the range ends there.
            prefilter
                .prefix(input.haystack, span)
                .filter(|found| found.end <= input.end)
        } else {
            prefilter.find(input.haystack, span)
        };
        found.map(|found| (found.start, found.end))
    }

    /// `search` for a pattern that can match nothing, which matches where the
    /// search starts. An empty match inside a UTF-8 sequence is not reported:
    /// as the meta engine does, an unanchored search then starts again a byte
    /// later. A match that is not empty is reported wherever it ends, as in
    /// byte modes it may end inside a sequence.
    fn search_empty<S: Slots + ?Sized>(
        &self,
        input: &PlanInput<'_>,
        earliest: bool,
        mut slots: Option<&mut S>,
    ) -> Option<(usize, usize)> {
        let mut start = input.start;
        loop {
            let end = self
                .attempt(input, start, earliest, slots.as_deref_mut())
                .ok()?;
            if end > start || char_boundary(input.haystack, end) {
                return Some((start, end));
            }
            if input.anchored || start >= input.end {
                return None;
            }
            start += 1;
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
            let candidate = self.next_candidate(input, from)?;
            match self.window_attempt(input, candidate, earliest, slots.as_deref_mut()) {
                Ok(found) => return Some(found),
                Err(resume) if resume >= input.end => return None,
                Err(resume) => from = resume,
            }
        }
    }
}

/// How an attempt stopped short of the end of the program.
enum Stop {
    /// It cannot match from its start, unless it recorded a match before.
    Fail,
    /// A membership test reached a point where a match ends.
    Found(usize),
}

/// A match an attempt reports if what follows fails: where a greedy group
/// could have stopped repeating.
struct Accept<'p> {
    group: &'p Group,
    at: usize,
}

/// An attempt in progress.
struct State<'p, 's, S: Slots + ?Sized> {
    pos: usize,
    slots: Option<&'s mut S>,
    accept: Option<Accept<'p>>,
    /// The values of the recorded group's snapshot slots, `usize::MAX` for
    /// none, when slots are recorded.
    saved: Option<[usize; SNAPSHOT]>,
}

impl<'p, S: Slots + ?Sized> State<'p, '_, S> {
    /// Record the current position as where `group` could stop.
    #[inline]
    fn record(&mut self, group: &'p Group) {
        if let Some(slots) = self.slots.as_deref() {
            let saved = self.saved.get_or_insert([usize::MAX; SNAPSHOT]);
            for (value, &slot) in saved.iter_mut().zip(&*group.snapshot) {
                *value = slots.get(slot).unwrap_or(usize::MAX);
            }
        }
        self.accept = Some(Accept {
            group,
            at: self.pos,
        });
    }

    /// The recorded match, with the slots restored to what they were and
    /// those the rest of the pattern sets when it matches nothing.
    fn fall_back(&mut self) -> Option<usize> {
        let Accept { group, at } = self.accept.take()?;
        if let Some(slots) = self.slots.as_deref_mut() {
            if let Some(saved) = &self.saved {
                for (&value, &slot) in saved.iter().zip(&*group.snapshot) {
                    match value {
                        usize::MAX => slots.clear(slot),
                        value => slots.set(slot, value),
                    }
                }
            }
            for &slot in &*group.completion {
                slots.set(slot, at);
            }
        }
        Some(at)
    }
}

/// An attempt's failure at a leading assertion, which says nothing about
/// later starts.
const LOOK_MISS: usize = usize::MAX;

/// Whether `bytes` occur at `at`, ending by `end`. A single byte, the
/// commonest literal between runs, is compared directly rather than through a
/// slice comparison call.
#[inline(always)]
fn literal_at(haystack: &[u8], at: usize, end: usize, bytes: &[u8]) -> bool {
    match bytes {
        [byte] => at < end && haystack[at] == *byte,
        _ => haystack
            .get(at..end)
            .is_some_and(|rest| rest.starts_with(bytes)),
    }
}

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
    fn get(&self, slot: usize) -> Option<usize>;
    fn clear(&mut self, slot: usize);
}

impl Slots for [Option<NonMaxUsize>] {
    fn set(&mut self, slot: usize, at: usize) {
        self[slot] = NonMaxUsize::new(at);
    }

    fn get(&self, slot: usize) -> Option<usize> {
        self[slot].map(NonMaxUsize::get)
    }

    fn clear(&mut self, slot: usize) {
        self[slot] = None;
    }
}

/// `usize::MAX` marks a group that did not take part.
impl Slots for [usize] {
    fn set(&mut self, slot: usize, at: usize) {
        self[slot] = at;
    }

    fn get(&self, slot: usize) -> Option<usize> {
        Some(self[slot]).filter(|&at| at != usize::MAX)
    }

    fn clear(&mut self, slot: usize) {
        self[slot] = usize::MAX;
    }
}

#[cfg(test)]
mod tests {
    use super::{NFA_UNIT_BUDGET, Plan, PlanInput, RESCAN_BOUND};

    #[cfg(feature = "std")]
    std::thread_local! {
        /// The bytes attempts have read past their starts, plus one each.
        pub(super) static COVERED: core::cell::Cell<usize> = const { core::cell::Cell::new(0) };
    }

    fn plan(pattern: &str) -> Option<Plan> {
        Plan::new(&regex_syntax::parse(pattern).unwrap(), true)
    }

    /// Shapes the plan takes, and those it leaves to an engine: overlapping
    /// follows, assertions a shorter run could satisfy, choices the next
    /// character cannot decide, alternatives or repetitions that can match
    /// nothing, rescans beyond the bound, and end anchors a reverse search
    /// finds sooner.
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
            r"\w*",
            "ab|cd",
            "Holmes|Watson|Lestrade",
            "ab|ac",
            "(?:a|)b",
            r"\b(?:fn|let|mut)\b",
            "(?i)holmes|watson",
            "(?:ab)+",
            "(?:ab){2,}",
            "colou?r",
            "(a)?(b*)",
            r"(\d+)(?:\.(\d+))?",
            r"\w+(?:\.\w+)*",
            "(?:foo|bar)(?:baz)?",
            "x(?:ab)*?y",
        ] {
            assert!(plan(pattern).is_some(), "{pattern}");
        }
        for pattern in [
            r"\w+s",
            r"\w+\B",
            r"(?m)[^a]+$",
            "[a-z]+?x",
            r"\d+$",
            "a{1,70}b",
            r"\w+-\w+-x",
            r"x[a-z]*\d",
            "ab+|ac",
            "(a|ab)c",
            "(?:a|)b+",
            "(?:a*)*",
            "(?:ab)*a",
            r"x(?:ab)*?\b",
            "(?:foo|bar)+qux",
            "a(?:ba)*c",
            r"\b(?:ab)*",
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

    /// Every admitted program reads each byte a bounded number of times over
    /// a whole iteration, on text built to make attempts fail late: repeated
    /// partial matches. Patterns whose attempts could fail after unbounded
    /// text holding other starts must have been rejected, which this checks
    /// by running whichever of them a weaker admission would take.
    #[cfg(feature = "std")]
    #[test]
    fn attempts_read_each_byte_a_bounded_number_of_times() {
        use alloc::string::String;
        let patterns = [
            r"\w+",
            r"\w+(?:\.\w+)*",
            r"(\d+)(?:\.(\d+))?",
            "(?:ab)+",
            "(?:ab)+c",
            "x(?:ab)*y",
            "(?:foo|bar)+qux",
            "(?:foo|bar)(?:baz)?",
            "a(?:ba)*c",
            r"\w+-\w+-x",
            r"x[a-z]*\d",
            "Holmes|Hound",
            "(?:a|b)+c",
            "(a)?(b*)",
            r"[a-c]{1,40}d",
            r"\w+(?:,\w+)*;",
            r"(?:\w\.)+x",
        ];
        let fragments = [
            "ab", "abab", "foo", "bar", "qu", "a.", "1.", "x", "ba", "-", "Ho", "c", "a,", "b",
            "q", ".", " ", "1",
        ];
        let mut seed = 7u64;
        let mut next = move |n: usize| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed % n as u64) as usize
        };
        for pattern in patterns {
            let Some(plan) = plan(pattern) else {
                continue;
            };
            for round in 0..40 {
                let mut text = String::new();
                let unit: String = (0..1 + round % 4)
                    .map(|_| fragments[next(fragments.len())])
                    .collect();
                while text.len() < 4000 {
                    if next(8) == 0 {
                        text.push_str(fragments[next(fragments.len())]);
                    } else {
                        text.push_str(&unit);
                    }
                }
                COVERED.with(|covered| covered.set(0));
                let mut start = 0;
                while start <= text.len() {
                    let input = PlanInput {
                        haystack: text.as_bytes(),
                        start,
                        end: text.len(),
                        anchored: false,
                        utf8: true,
                    };
                    match plan.find::<[usize]>(&input, None) {
                        Some((_, end)) => start = if end > start { end } else { start + 1 },
                        None => break,
                    }
                }
                let covered = COVERED.with(core::cell::Cell::get);
                assert!(
                    covered <= (RESCAN_BOUND + 8) * (text.len() + 1),
                    "{pattern}: {covered} bytes read for {} on {unit:?}",
                    text.len()
                );
            }
        }
    }
}
