// Copyright 2026 The Fancy Regex Authors.
//
// Permission is hereby granted, free of charge, to any person obtaining a copy
// of this software and associated documentation files (the "Software"), to deal
// in the Software without restriction, including without limitation the rights
// to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
// copies of the Software, and to permit persons to whom the Software is
// furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in
// all copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
// IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
// OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN
// THE SOFTWARE.

//! Seek pre-filter: build a simplified approximation of a pattern used to
//! skip to plausible match positions before handing off to the backtracking VM.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

#[cfg(not(feature = "std"))]
use alloc::collections::BTreeMap as Map;
#[cfg(feature = "std")]
use std::collections::HashMap as Map;

use alloc::boxed::Box;
use core::convert::TryFrom;
use regex_automata::util::prefilter::Prefilter;
use regex_automata::{MatchKind, Span};
use regex_syntax::hir::literal::{ExtractKind, Extractor};
use regex_syntax::hir::{Class, ClassUnicode, ClassUnicodeRange, Hir, Repetition};

use crate::analyze::Info;
use crate::compile::MAX_SUBROUTINE_RECURSION_DEPTH;
use crate::{Absent, Assertion, Expr, write_quantifier};

/// Returns `true` if the expression tree contains a `StartText` or `EndText` assertion anywhere.
///
/// This is used to decide whether it is safe to inline a group body when approximating a backref:
/// text-anchor assertions in an inlined body would be evaluated at the wrong string position and
/// could cause false negatives (missing valid match positions).
pub(crate) fn expr_contains_positional_anchor(expr: &Expr) -> bool {
    match expr {
        // StartText (^), EndText ($), StartLine ((?m)^), EndLine ((?m)$) are positional anchors
        // that depend on absolute position in the string.  When a group containing such anchors
        // is inlined for a backref approximation, the anchors would be evaluated at the wrong
        // position (the backref position rather than the original capture position), potentially
        // producing false negatives.
        Expr::Assertion(
            Assertion::StartText
            | Assertion::EndText
            | Assertion::EndTextIgnoreTrailingNewlines { .. }
            | Assertion::StartLine { .. }
            | Assertion::StartLineOniguruma { .. }
            | Assertion::EndLine { .. },
        ) => true,
        _ => expr.children_iter().any(expr_contains_positional_anchor),
    }
}

/// Cap on the seek-pattern buffer length. Inlining a backref/subroutine
/// substitutes the referenced group's body, and when that body transitively
/// references the same group (e.g. `(\s+((\3|\4|\5)))?` where `\4`/`\5` point
/// at ancestors of the backref) the substitution branches at every level and,
/// bounded only by the recursion depth, expands the buffer exponentially — so
/// a short pattern can produce a multi-megabyte seek string. Once the buffer
/// is already this large, further inlining stops and a permissive placeholder
/// is emitted instead: the seek pattern only narrows candidate start positions
/// and never decides matches, so a less-selective approximation stays correct.
pub(crate) const MAX_SEEK_PATTERN_LEN: usize = 4096;

/// Emit a permissive size placeholder — `(?s:.)+` or `(?s:.){N,}` — into `buf`.
///
/// Used when a hard node cannot be represented in the seek pattern but has a known minimum
/// size.  The placeholder is an over-approximation that avoids false negatives: it admits any
/// run of characters of at least `min_size` length.
pub(crate) fn emit_min_size_placeholder(buf: &mut String, min_size: usize, precedence: u8) {
    if min_size == 0 {
        return; // zero width: safe to drop without emitting anything
    }
    if precedence > 2 {
        buf.push_str("(?:");
    }
    buf.push_str("(?s:.)");
    match min_size {
        1 => buf.push('+'),
        n => {
            buf.push('{');
            crate::push_usize(buf, n);
            buf.push_str(",}");
        }
    }
    if precedence > 2 {
        buf.push(')');
    }
}

/// Write the seek-pattern approximation for `info` into `buf`.
///
/// Easy subtrees without capture groups are serialised verbatim via [`Expr::to_str`].
/// Hard nodes are handled as follows:
/// - `Backref` / `SubroutineCall`: inline the referenced group's body (up to
///   `MAX_SUBROUTINE_RECURSION_DEPTH`). For backrefs, positional anchors (`^`, `$`, `\b`) etc.
///   are dropped from the inlined body because backreferences only capture text, not positions.
///   If inlining is not possible, a permissive `(?s:.){min_size,}` placeholder is emitted
///   instead of silently dropping, to maintain correctness.
/// - `LookAround`, `KeepOut`, `ContinueFromPreviousMatchEnd`, `BacktrackingControlVerb`,
///   `BackrefExistsCondition`, `Absent`: dropped (emit nothing — safe over-approximation,
///   their `min_size` is 0).
/// - `AtomicGroup`: keep the contents, discard the atomicity wrapper.
/// - `Group`: keep the contents, discard the capture group wrapper.
/// - `Conditional`: emit the union of the true and false branches.
/// - `GeneralNewline`: replaced with an explicit alternation.
/// - `Assertion`: anchors and word-boundary assertions are emitted; half-boundaries are
///   dropped (over-approximation — `\b` would be too restrictive).
pub(crate) fn build_seek_pattern<'a>(
    info: &Info<'a>,
    group_info_map: &Map<usize, &'a Info<'a>>,
    depth: usize,
    buf: &mut String,
    precedence: u8,
) {
    let mut inlined_groups = Vec::new();
    build_seek_pattern_impl(
        info,
        group_info_map,
        depth,
        buf,
        precedence,
        false,
        &mut inlined_groups,
    );
}

pub(crate) fn build_seek_pattern_impl<'a>(
    info: &Info<'a>,
    group_info_map: &Map<usize, &'a Info<'a>>,
    depth: usize,
    buf: &mut String,
    precedence: u8,
    drop_positional_anchors: bool,
    inlined_groups: &mut Vec<usize>,
) {
    // Drop positional anchors at this node when requested (used when inlining for a backref).
    if drop_positional_anchors {
        if let Expr::Assertion(
            Assertion::StartText
            | Assertion::EndText
            | Assertion::StartLine { .. }
            | Assertion::StartLineOniguruma { .. }
            | Assertion::EndLine { .. },
        ) = info.expr
        {
            return;
        }
        // \Z is an odd one out, although it is a zero-width assertion, for the purposes of seeking
        // it still needs to consume the newlines
        if let Expr::Assertion(Assertion::EndTextIgnoreTrailingNewlines { crlf }) = info.expr {
            if *crlf {
                buf.push_str(r"[\r\n]*");
            } else {
                buf.push_str(r"\n*");
            }
            return;
        }
    }

    let has_capture_groups = info.start_group() != info.end_group();
    if !info.hard && !has_capture_groups {
        // Easy subtree — use to_str directly when no positional-anchor dropping is needed,
        // or when the subtree contains no positional anchors (so dropping is a no-op).
        if !drop_positional_anchors || !expr_contains_positional_anchor(info.expr) {
            info.expr.to_str(buf, precedence);
            return;
        }
        // The easy subtree contains positional anchors that need to be stripped.
        // Fall through to the match below, which recurses via build_seek_pattern_impl.
    }

    match info.expr {
        Expr::Empty | Expr::DefineGroup { .. } => {}
        Expr::Assertion(assertion) => {
            // Emit all assertion types, approximating half-word-boundaries with \b.
            // Note: easy positional assertions (StartText, EndText, StartLine, EndLine) are
            // handled by the to_str early return above (drop_positional_anchors=false) or by
            // the early drop at the top of this function (drop_positional_anchors=true).
            // Only hard assertions reach this arm.
            match assertion {
                Assertion::EndTextIgnoreTrailingNewlines { crlf: false } => buf.push_str(r"\n*$"),
                Assertion::EndTextIgnoreTrailingNewlines { crlf: true } => {
                    buf.push_str(r"[\r\n]*$")
                }
                Assertion::WordBoundary => buf.push_str(r"\b"),
                Assertion::NotWordBoundary => buf.push_str(r"\B"),
                // Full word boundaries: \< and \> — overapproximate with \b.
                Assertion::LeftWordBoundary | Assertion::RightWordBoundary => buf.push_str(r"\b"),
                // Half-boundaries (\b{start-half}, \b{end-half}) can match in positions where
                // \b does not (e.g. before punctuation), so \b would be an under-approximation.
                // Drop them (emit nothing) which is a safe over-approximation.
                Assertion::LeftWordHalfBoundary | Assertion::RightWordHalfBoundary => {}
                // Easy positional anchors — handled by early return / early drop above.
                // They can only reach here when drop_positional_anchors=false and the subtree
                // is easy (handled by to_str) but the Group/Concat parent fell through to match.
                // Emit them faithfully.
                Assertion::StartText => buf.push('^'),
                Assertion::EndText => buf.push('$'),
                Assertion::StartLine { crlf: false }
                | Assertion::StartLineOniguruma { crlf: false } => buf.push_str("(?m:^)"),
                Assertion::StartLine { crlf: true }
                | Assertion::StartLineOniguruma { crlf: true } => buf.push_str("(?Rm:^)"),
                Assertion::EndLine { crlf: false } => buf.push_str("(?m:$)"),
                Assertion::EndLine { crlf: true } => buf.push_str("(?Rm:$)"),
            }
        }
        Expr::Concat(_) => {
            if precedence > 1 {
                buf.push_str("(?:");
            }
            for child in &info.children {
                build_seek_pattern_impl(
                    child,
                    group_info_map,
                    depth,
                    buf,
                    2,
                    drop_positional_anchors,
                    inlined_groups,
                );
            }
            if precedence > 1 {
                buf.push(')');
            }
        }
        Expr::Alt(_) => {
            if precedence > 0 {
                buf.push_str("(?:");
            }
            let mut first = true;
            for child in &info.children {
                if !first {
                    buf.push('|');
                }
                build_seek_pattern_impl(
                    child,
                    group_info_map,
                    depth,
                    buf,
                    1,
                    drop_positional_anchors,
                    inlined_groups,
                );
                first = false;
            }
            if precedence > 0 {
                buf.push(')');
            }
        }
        Expr::Group(_) => {
            // Drop the capture group wrapper; keep the contents.
            if !info.children.is_empty() {
                build_seek_pattern_impl(
                    &info.children[0],
                    group_info_map,
                    depth,
                    buf,
                    precedence,
                    drop_positional_anchors,
                    inlined_groups,
                );
            }
        }
        Expr::Repeat { lo, hi, greedy, .. } => {
            if precedence > 2 {
                buf.push_str("(?:");
            }
            if !info.children.is_empty() {
                build_seek_pattern_impl(
                    &info.children[0],
                    group_info_map,
                    depth,
                    buf,
                    3,
                    drop_positional_anchors,
                    inlined_groups,
                );
            }
            write_quantifier(buf, *lo, *hi, *greedy);
            if precedence > 2 {
                buf.push(')');
            }
        }
        Expr::Backref { group, casei }
        | Expr::BackrefWithRelativeRecursionLevel { group, casei, .. } => {
            // Inline the body of the referenced capture group, wrapping with (?i:...) when
            // the backref is case-insensitive so the approximation remains correct.
            //
            // Positional anchors are dropped from the inlined body: a backref matches the
            // captured text, not a position, so anchors from the group definition do not hold
            // at the backref's position.
            //
            // For BackrefWithRelativeRecursionLevel, the relative_level is ignored here:
            // for seeking purposes, the group body is the same regardless of recursion level.
            //
            // If inlining is not possible (depth limit, group not in map, or recursive cycle), emit a permissive
            // placeholder so that no match positions are incorrectly skipped.
            if depth < MAX_SUBROUTINE_RECURSION_DEPTH
                && buf.len() < MAX_SEEK_PATTERN_LEN
                && let Some(group_info) = group_info_map.get(group)
            {
                if inlined_groups.contains(group) {
                    emit_min_size_placeholder(buf, group_info.min_size, precedence);
                    return;
                }
                if !group_info.children.is_empty() {
                    let child = &group_info.children[0];
                    inlined_groups.push(*group);
                    if *casei {
                        let mut inner = String::new();
                        build_seek_pattern_impl(
                            child,
                            group_info_map,
                            depth + 1,
                            &mut inner,
                            // Precedence 0 (alternation) so content inside (?i:...) is unambiguous.
                            0,
                            true,
                            inlined_groups,
                        );
                        inlined_groups.pop();
                        if !inner.is_empty() {
                            buf.push_str("(?i:");
                            buf.push_str(&inner);
                            buf.push(')');
                        }
                    } else {
                        build_seek_pattern_impl(
                            child,
                            group_info_map,
                            depth + 1,
                            buf,
                            precedence,
                            true,
                            inlined_groups,
                        );
                        inlined_groups.pop();
                    }
                    return;
                }
                // Empty group (no children): min_size=0, nothing to emit.
                return;
            }
            // Could not inline (depth limit or group not found): emit a permissive placeholder
            // so we don't falsely skip positions where the backref's target could have matched.
            emit_min_size_placeholder(buf, info.min_size, precedence);
        }
        Expr::SubroutineCall(target_group) => {
            // Inline the body of the target group, honouring the recursion depth limit.
            if depth < MAX_SUBROUTINE_RECURSION_DEPTH
                && buf.len() < MAX_SEEK_PATTERN_LEN
                && let Some(group_info) = group_info_map.get(target_group)
            {
                if !group_info.children.is_empty() {
                    build_seek_pattern_impl(
                        &group_info.children[0],
                        group_info_map,
                        depth + 1,
                        buf,
                        precedence,
                        drop_positional_anchors,
                        inlined_groups,
                    );
                    return;
                }
                return;
            }
            emit_min_size_placeholder(buf, info.min_size, precedence);
        }
        // LookAround is zero-width — drop it.
        Expr::LookAround(_, _) => {}
        Expr::AtomicGroup(_) => {
            // Keep the contents; atomicity is invisible to the seek approximation.
            if !info.children.is_empty() {
                build_seek_pattern_impl(
                    &info.children[0],
                    group_info_map,
                    depth,
                    buf,
                    precedence,
                    drop_positional_anchors,
                    inlined_groups,
                );
            }
        }
        Expr::GeneralNewline { unicode } => {
            // Replace \R with an explicit alternation accepted by regex-automata.
            if *unicode {
                buf.push_str(r"(?:\r\n|[\n\x0B\x0C\r\x85\u{2028}\u{2029}])");
            } else {
                buf.push_str(r"(?:\r\n|[\n\x0B\x0C\r])");
            }
        }
        Expr::Conditional { .. } => {
            // Emit the union of (condition + true_branch) and (false_branch).
            //
            // Including the condition prefix in the true alternative ensures that consuming
            // conditions (e.g. `(?(a)b|c)` where `a` is matched and consumed) are
            // over-approximated correctly.  For zero-width conditions (e.g. backref-exists
            // checks) the condition emits nothing and the result degenerates to
            // `true_branch | false_branch`, preserving the original behaviour.
            let mut cond_pat = String::new();
            let mut true_pat = String::new();
            let mut false_pat = String::new();
            if !info.children.is_empty() {
                build_seek_pattern_impl(
                    &info.children[0],
                    group_info_map,
                    depth,
                    &mut cond_pat,
                    2,
                    drop_positional_anchors,
                    inlined_groups,
                );
            }
            if info.children.len() >= 2 {
                build_seek_pattern_impl(
                    &info.children[1],
                    group_info_map,
                    depth,
                    &mut true_pat,
                    2,
                    drop_positional_anchors,
                    inlined_groups,
                );
            }
            if info.children.len() >= 3 {
                build_seek_pattern_impl(
                    &info.children[2],
                    group_info_map,
                    depth,
                    &mut false_pat,
                    1,
                    drop_positional_anchors,
                    inlined_groups,
                );
            }
            // Build the "condition then true-branch" alternative.
            let cond_true = if cond_pat.is_empty() {
                true_pat.clone()
            } else if true_pat.is_empty() {
                cond_pat.clone()
            } else {
                format!("(?:{}{})", cond_pat, true_pat)
            };
            match (cond_true.is_empty(), false_pat.is_empty()) {
                (true, true) => {}
                // An empty approximation matches the empty string, so the
                // union must keep that alternative rather than require the
                // other branch.
                (true, false) | (false, true) => {
                    buf.push_str("(?:");
                    buf.push_str(if cond_true.is_empty() {
                        &false_pat
                    } else {
                        &cond_true
                    });
                    buf.push_str("|)");
                }
                (false, false) => {
                    if precedence > 0 {
                        buf.push_str("(?:");
                    }
                    buf.push_str(&cond_true);
                    buf.push('|');
                    buf.push_str(&false_pat);
                    if precedence > 0 {
                        buf.push(')');
                    }
                }
            }
        }
        // Absent repeater `(?~expr)` matches any content not containing `expr`.
        // Approximate with `(?s:.*)` (any characters, including newline) since the repeater
        // can consume an arbitrary number of characters.
        Expr::Absent(Absent::Repeater(_)) => buf.push_str("(?s:.*)"),
        // Absent expression `(?~|absent|exp)` matches `exp` subject to the absent constraint.
        // Approximate by seeking only for `exp`, dropping the constraint — this is a safe
        // over-approximation since any position where `exp` matches (ignoring the constraint)
        // is a superset of positions where the full expression matches.
        Expr::Absent(crate::Absent::Expression { .. }) => {
            if info.children.len() >= 2 {
                build_seek_pattern_impl(
                    &info.children[1],
                    group_info_map,
                    depth,
                    buf,
                    precedence,
                    drop_positional_anchors,
                    inlined_groups,
                );
            }
        }
        // Zero-width / control nodes — drop them (all have min_size = 0).
        Expr::KeepOut
        | Expr::ContinueFromPreviousMatchEnd
        | Expr::BacktrackingControlVerb(_)
        | Expr::BackrefExistsCondition { .. }
        | Expr::Absent(_) => {}
        // Easy leaf nodes (Literal, Any, Delegate) are always handled by the easy no-captures
        // early return above and never reach here. Listed explicitly so that adding a new
        // Expr variant produces a compile error until the seek-pattern case is handled.
        Expr::Literal { .. } | Expr::Any { .. } | Expr::Delegate { .. } => {
            info.expr.to_str(buf, precedence)
        }
        // These variants cause a compile error during analysis and are therefore unreachable
        // after a successful `analyze()` call.
        Expr::AstNode(..) => {
            unreachable!("unexpected expr variant after analysis")
        }
    }
}

/// Where an unanchored VM attempt can begin; see [`candidate_starts`].
#[derive(Debug)]
pub(crate) enum StartFilter {
    /// memchr or memmem through regex-automata.
    Literal(Prefilter),
    /// More first bytes than memchr handles, or no memchr support. For these
    /// `Prefilter::new` would build Teddy at many times the construction
    /// cost; this is regex-automata's own byte-set strategy, which it does
    /// not expose.
    Bytes(Box<[bool; 256]>),
}

impl StartFilter {
    /// The first position at or after `start` where an attempt can begin.
    pub(crate) fn find(&self, haystack: &[u8], start: usize) -> Option<usize> {
        match self {
            Self::Literal(prefilter) => prefilter
                .find(haystack, Span::from(start..haystack.len()))
                .map(|span| span.start),
            Self::Bytes(set) => haystack[start..]
                .iter()
                .position(|&byte| set[usize::from(byte)])
                .map(|offset| start + offset),
        }
    }
}

/// Build a candidate-position filter for unanchored VM searches.
///
/// [`Leading`] over-approximates the text an attempt consumes first, so
/// every match starts at an occurrence of one of the prefix literals that
/// regex-syntax extracts from it. Only searchers that are nearly free to build
/// are used: memmem for a common prefix of at least two bytes, memchr for at
/// most three distinct first bytes, otherwise a first-byte table. Literals are
/// built from whole characters, so each one begins on a character boundary.
pub(crate) fn candidate_starts(info: &Info<'_>) -> Option<StartFilter> {
    let mut leading = Leading::default();
    let complete = leading.visit(info);
    let mut prefixes = Extractor::new()
        .kind(ExtractKind::Prefix)
        .extract(&leading.into_hir());
    if !complete {
        prefixes.make_inexact();
    }
    let literals = prefixes.literals()?;
    let common = prefixes.longest_common_prefix()?;
    let mut first_bytes = Vec::new();
    for literal in literals {
        let &first = literal.as_bytes().first()?;
        if !first_bytes.contains(&first) {
            first_bytes.push(first);
        }
    }
    let literal = if common.len() >= 2 {
        Prefilter::new(MatchKind::LeftmostFirst, &[common])
    } else if first_bytes.len() <= 3 {
        Prefilter::new(
            MatchKind::LeftmostFirst,
            &first_bytes.chunks(1).collect::<Vec<_>>(),
        )
    } else {
        None
    };
    Some(literal.map_or_else(
        || {
            let mut set = Box::new([false; 256]);
            for byte in first_bytes {
                set[usize::from(byte)] = true;
            }
            StartFilter::Bytes(set)
        },
        StartFilter::Literal,
    ))
}

/// An over-approximation, as an `Hir`, of the text an attempt consumes first.
#[derive(Default)]
struct Leading {
    parts: Vec<Hir>,
    /// Adjacent literal bytes, joined into one node; the parser yields one
    /// `Literal` per character.
    literal: Vec<u8>,
}

impl Leading {
    fn push(&mut self, hir: Hir) {
        self.flush();
        self.parts.push(hir);
    }

    fn flush(&mut self) {
        if !self.literal.is_empty() {
            self.parts
                .push(Hir::literal(core::mem::take(&mut self.literal)));
        }
    }

    fn into_hir(mut self) -> Hir {
        self.flush();
        Hir::concat(self.parts)
    }

    /// Append what `info` consumes. Returns `false` once arbitrary text has
    /// to be assumed for the remainder; that stop always propagates to the
    /// caller, which then marks every literal inexact. Only literal and
    /// structural nodes are translated, which bounds the work to the leading
    /// part of the pattern.
    fn visit(&mut self, info: &Info<'_>) -> bool {
        match info.expr {
            Expr::Empty
            | Expr::Assertion(_)
            | Expr::LookAround(..)
            | Expr::KeepOut
            | Expr::ContinueFromPreviousMatchEnd
            | Expr::BacktrackingControlVerb(_)
            | Expr::DefineGroup { .. } => true,
            Expr::Literal { val, casei: false } => {
                self.literal.extend_from_slice(val.as_bytes());
                true
            }
            Expr::Literal { val, casei: true } => {
                // ASCII non-letters have no case variants. The first character
                // that may is expanded by simple case folding (a superset of
                // ASCII-only folding), which fixes the first bytes; the
                // approximation ends there.
                let Some((index, cased)) = val
                    .char_indices()
                    .find(|&(_, c)| !c.is_ascii() || c.is_ascii_alphabetic())
                else {
                    self.literal.extend_from_slice(val.as_bytes());
                    return true;
                };
                self.literal.extend_from_slice(&val.as_bytes()[..index]);
                let mut folds = ClassUnicode::new([ClassUnicodeRange::new(cased, cased)]);
                if folds.try_case_fold_simple().is_ok() {
                    self.push(Hir::class(Class::Unicode(folds)));
                }
                false
            }
            Expr::Concat(_) => info.children.iter().all(|child| self.visit(child)),
            Expr::Group(_) | Expr::AtomicGroup(_) => self.visit(&info.children[0]),
            Expr::Alt(_) => {
                let mut complete = true;
                let branches = info
                    .children
                    .iter()
                    .map(|child| {
                        let mut branch = Leading::default();
                        complete &= branch.visit(child);
                        branch.into_hir()
                    })
                    .collect();
                self.push(Hir::alternation(branches));
                complete
            }
            Expr::Repeat { lo, hi, greedy, .. } => {
                let mut body = Leading::default();
                if !body.visit(&info.children[0]) {
                    // The first iteration fixes the prefix; an optional one
                    // leaves it to the unknown remainder.
                    if *lo > 0 {
                        self.push(body.into_hir());
                    }
                    return false;
                }
                let max = match *hi {
                    usize::MAX => None,
                    hi => match u32::try_from(hi) {
                        Ok(hi) => Some(hi),
                        Err(_) => return false,
                    },
                };
                let Ok(min) = u32::try_from(*lo) else {
                    return false;
                };
                self.push(Hir::repetition(Repetition {
                    min,
                    max,
                    greedy: *greedy,
                    sub: Box::new(body.into_hir()),
                }));
                true
            }
            // Classes, backreferences, conditionals, subroutines and the like:
            // anything may follow.
            _ => false,
        }
    }
}

/// Returns `true` if `pattern` contains at least one character that provides useful filtering
/// (i.e. a literal character, character class, or anchor rather than just wildcards/quantifiers).
///
/// This is a conservative approximation: any pattern containing `[`, `\`, `^`, `$`, or an
/// alphanumeric character is considered useful. The primary goal is to reject fully
/// unconstrained patterns such as `.*`.
pub fn seek_pattern_is_useful(pattern: &str) -> bool {
    pattern
        .bytes()
        .any(|b| matches!(b, b'[' | b'\\' | b'^' | b'$' | b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analyze::{AnalyzeContext, analyze};
    use crate::compile::populate_group_info_map;
    use crate::optimize;
    use crate::{BytesMode, Regex};

    /// Build the seek pattern for a regex string and return it.
    fn get_seek_pattern(re: &str) -> String {
        let mut tree = Expr::parse_tree(re).unwrap();
        let requires_capture_group_fixup = optimize(&mut tree);

        let info = analyze(
            &tree,
            AnalyzeContext {
                explicit_capture_group_0: requires_capture_group_fixup,
                ..AnalyzeContext::default()
            },
        )
        .unwrap();

        let mut group_info_map = Map::new();
        populate_group_info_map(&mut group_info_map, &info);
        let mut buf = String::new();
        build_seek_pattern(&info, &group_info_map, 0, &mut buf, 0);
        buf
    }

    #[test]
    fn seek_pattern_backref_no_anchor() {
        // Simple backref with no positional anchors: group body is inlined verbatim.
        // The `(?:...)` wrapping comes from Concat precedence handling when stripping groups.
        assert_eq!(get_seek_pattern(r"(abc)\1"), "(?:abc)(?:abc)");
        assert_eq!(
            get_seek_pattern(r"(?i)(abc)\1"),
            "(?:(?i:a)(?i:b)(?i:c))(?i:(?i:a)(?i:b)(?i:c))"
        );
    }

    #[test]
    fn seek_pattern_backref_with_start_anchor() {
        // Backref to a group whose body starts with `^`: the anchor should be dropped from the
        // inlined backref position but kept at the group definition site.
        // `(^a)\1` — seek = `(?:^a)` (group) + `(?:a)` (backref, anchor dropped)
        assert_eq!(get_seek_pattern(r"(^a)\1"), "(?:^a)(?:a)");
    }

    #[test]
    fn seek_pattern_backref_with_start_anchor_variable_length() {
        // The group has a start anchor plus a variable-length content.
        // Anchor is dropped when inlining for the backref.
        assert_eq!(get_seek_pattern(r"(^a+)\1"), "(?:^a+)(?:a+)");
    }

    #[test]
    fn seek_pattern_backref_with_end_anchor() {
        // Backref to a group ending with `$`: the anchor is dropped in the inline position.
        assert_eq!(get_seek_pattern(r"(a$)\1"), "(?:a$)(?:a)");
    }

    #[test]
    fn seek_pattern_backref_only_anchor() {
        // Backref to a group that is only a positional anchor.
        // The group emits `^` (min_size=0); the backref drops `^` and emits nothing.
        assert_eq!(get_seek_pattern(r"(^)\1"), "^");
    }

    #[test]
    fn seek_pattern_lookahead_dropped() {
        // Lookahead is zero-width and is dropped; only the literal is kept.
        assert_eq!(get_seek_pattern(r"(?=foo)bar"), "bar");
    }

    #[test]
    fn seek_pattern_casei_literal_backref_with_anchor() {
        // Group contains a case-insensitive literal and a start anchor.
        // The anchor is dropped during backref inlining; the casei literal emits (?i:...).
        assert_eq!(get_seek_pattern(r"(?i:(^a))\1"), "(?:^(?i:a))(?:(?i:a))");
    }

    #[test]
    fn seek_pattern_easy_expr_preserved() {
        // Easy (non-hard) expressions without capture groups are serialised verbatim via to_str.
        assert_eq!(get_seek_pattern(r"abc"), "abc");
        assert_eq!(get_seek_pattern(r"a|b"), "a|b");
        assert_eq!(get_seek_pattern(r"a+b*c?"), "a+b*c?");
    }

    #[test]
    fn seek_pattern_easy_expr_with_capture_group_strips_group_wrapper() {
        // Easy expressions with capture groups drop the capture group wrappings.
        assert_eq!(get_seek_pattern(r"(abc)"), "abc");
        assert_eq!(
            get_seek_pattern(r"(abc(def))(?<named>ghi)"),
            "(?:abc(?:def))(?:ghi)"
        );
    }

    #[test]
    fn seek_pattern_optimized_easy_expr_strips_group_wrapper() {
        // Optimized patterns lose the capture group wrappings
        assert_eq!(get_seek_pattern(r"(?=abc)"), "(?:abc)");
        assert_eq!(
            get_seek_pattern(r"(h)(e)(l)(l)(o)\s*(?=world)"),
            r"(?:hello\s*)(?:world)"
        );
    }

    #[test]
    fn seek_pattern_end_text_ignore_trailing_newlines_non_crlf() {
        // \Z (non-CRLF mode) — approximate with `\n*$` so the seek only visits positions
        // near end-of-text.
        assert_eq!(get_seek_pattern(r"abc\Z"), r"abc\n*$");
    }

    #[test]
    fn seek_pattern_end_text_ignore_trailing_newlines_crlf() {
        // \Z in CRLF mode — approximate with `(?:\r?\n)*$`.
        assert_eq!(get_seek_pattern(r"(?R)abc\Z"), r"abc[\r\n]*$");
    }

    #[test]
    fn seek_pattern_end_text_ignore_trailing_newlines_only() {
        // \Z alone (hard assertion) — just the seek pattern with no surrounding literal.
        assert_eq!(get_seek_pattern(r"\Z"), r"\n*$");
    }

    #[test]
    fn seek_pattern_backref_with_end_text_ignore_trailing_newlines() {
        // Backref to a group that ends with \Z: \Z is a positional anchor and should be
        // dropped when inlining the group body for the backref.
        // `(a\Z)\1` — seek = `(?:a\n*$)` (group) + `(?:a\n*)` (backref, \Z anchor dropped, newline matching kept)
        assert_eq!(get_seek_pattern(r"(a\Z)\1"), r"(?:a\n*$)(?:a\n*)");
    }

    #[test]
    fn seek_pattern_subroutine_call_inlined() {
        assert_eq!(
            get_seek_pattern(r"([A-Z][a-z0-9]*)::\g<1>"),
            r"(?:[A-Z][a-z0-9]*)::(?:[A-Z][a-z0-9]*)"
        );
    }

    #[test]
    fn seek_pattern_subroutine_call_inlined_and_anchors_are_preserved() {
        assert_eq!(
            get_seek_pattern(r"((?m:^)[A-Z][a-z0-9]*)\n\g<1>"),
            "(?:(?m:^)[A-Z][a-z0-9]*)\\n(?:(?m:^)[A-Z][a-z0-9]*)"
        );
    }

    #[test]
    fn seek_pattern_conditional_keeps_empty_branch() {
        assert_eq!(get_seek_pattern(r"(a)?(?(1)b)c"), "a?(?:b|)c");
        assert_eq!(get_seek_pattern(r"(a)?(?(1)(?=b)|c)"), "a?(?:c|)");
        // Seek and RegexSet candidates rely on the same approximation.
        let seek = crate::RegexBuilder::new(r"(a)?(?(1)b)c")
            .seek(true)
            .build()
            .unwrap();
        assert_eq!(seek.find("c").unwrap().map(|m| m.range()), Some(0..1));
        let set = crate::RegexSet::new([r"(a)?(?(1)b)c"]).unwrap();
        let mut matches = set
            .find_input(crate::RegexInput::new("c"))
            .unwrap()
            .unwrap();
        assert_eq!(matches.next().unwrap().unwrap().end(), 1);
    }

    #[test]
    fn seek_pattern_self_referential_backref_is_bounded() {
        // Inlining a backref whose target transitively references the backref's
        // own ancestors would expand exponentially with recursion depth if we
        // didn't have cycle detection or a length cap to cause it to fall back
        // to a permissive placeholder instead of continuing to recurse.
        // Together these help to keep the seek pattern small (and, therefore, compilation
        // fast) rather than producing a multi-megabyte string which would likely not help
        // to narrow candidate start positions much better anyway.
        let seek = get_seek_pattern(r"(end)(\s+(function))?(\s+((\3|\4|\5)))?");
        assert!(
            seek.len() <= MAX_SEEK_PATTERN_LEN,
            "seek pattern should stay bounded, got {} bytes",
            seek.len()
        );
        // Compiling it must succeed and stay well-formed.
        Regex::new(r"(end)(\s+(function))?(\s+((\3|\4|\5)))?").unwrap();
    }

    /// Spans of every match and capture group, then `find`'s span; `Err` when
    /// either search fails. Expands for both this crate and the oracle.
    macro_rules! observe {
        ($regex:expr, $text:expr) => {
            (|| -> core::result::Result<Vec<Vec<Option<(usize, usize)>>>, ()> {
                let mut all = Vec::new();
                for captures in $regex.captures_iter($text) {
                    let captures = captures.map_err(drop)?;
                    all.push(
                        captures
                            .iter()
                            .map(|m| m.map(|m| (m.start(), m.end())))
                            .collect(),
                    );
                }
                let found = $regex.find($text).map_err(drop)?;
                all.push(alloc::vec![found.map(|m| (m.start(), m.end()))]);
                Ok(all)
            })()
        };
    }

    fn start_filter(regex: &Regex) -> Option<&StartFilter> {
        match &regex.inner {
            crate::RegexImpl::Fancy { prog, .. } => prog.start_filter(),
            crate::RegexImpl::Wrap { .. } => None,
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
        let mut texts = alloc::vec![
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
    fn candidate_starts_keep_every_oracle_match_start() {
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
        ];
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        let mut patterns: Vec<String> = fixed.iter().map(|p| String::from(*p)).collect();
        patterns.extend((0..2000).map(|_| generated_pattern(&mut rng, 0)));
        let texts = generated_texts(&mut rng);
        let (mut rich, mut accelerated, mut checked_starts) = (0, 0, 0);
        for (index, pattern) in patterns.iter().enumerate() {
            let casei = index % 3 == 2;
            let built = crate::RegexBuilder::new(pattern)
                .case_insensitive(casei)
                .build();
            let reference = oracle::RegexBuilder::new(pattern)
                .case_insensitive(casei)
                .build();
            let (regex, reference) = match (built, reference) {
                (Ok(regex), Ok(reference)) => (regex, reference),
                (Err(_), Err(_)) => continue,
                (built, reference) => panic!(
                    "{pattern:?} compile disagreement: {:?} vs {:?}",
                    built.err(),
                    reference.err()
                ),
            };
            if matches!(regex.inner, crate::RegexImpl::Fancy { .. }) {
                rich += 1;
            }
            let filter = start_filter(&regex);
            accelerated += usize::from(filter.is_some());
            for text in &texts {
                assert_eq!(
                    observe!(regex, text),
                    observe!(reference, text),
                    "{pattern:?} casei={casei} {text:?}"
                );
                let Some(filter) = filter else { continue };
                // Judge the filter directly: an anchored search never consults
                // it, so a skipped viable start cannot hide behind the matcher.
                for (start, _) in text.char_indices().chain([(text.len(), ' ')]) {
                    let input = crate::RegexInput::new(text.as_str())
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
        // A mostly-fallback corpus would not exercise the skipping path.
        assert!(
            rich >= 800 && accelerated >= 300,
            "{accelerated}/{rich} accelerated"
        );
        assert!(
            checked_starts >= 3_000,
            "{checked_starts} viable starts checked"
        );
    }

    #[test]
    fn candidate_skipping_follows_whole_character_stepping_on_bytes() {
        // Whole-character stepping from an invalid lead byte never visits
        // offset 1, so an `A` there must not become a match.
        let haystack: &[u8] = b"\xE2Ac\xF0A";
        for mode in [
            BytesMode::Unicode,
            BytesMode::UnicodeBytes,
            BytesMode::Ascii,
        ] {
            let build = |limit: bool| {
                let mut builder = crate::RegexBuilder::new(r"A(?!b)");
                builder.bytes_mode(mode);
                if limit {
                    builder.backtrack_limit(1_000_000);
                }
                builder.build().unwrap()
            };
            let (skipping, exhaustive) = (build(false), build(true));
            assert!(start_filter(&skipping).is_some() && start_filter(&exhaustive).is_none());
            let spans = |regex: &Regex| -> Vec<_> {
                regex
                    .find_iter(haystack)
                    .map(|m| m.unwrap().range())
                    .collect()
            };
            assert_eq!(spans(&skipping), spans(&exhaustive), "{mode:?}");
        }
    }

    #[test]
    fn default_limit_skips_unviable_starts_but_explicit_limit_counts_them() {
        let haystack = "a".repeat(1_100_000);
        let skipping = Regex::new(r"#(\d)\1").unwrap();
        assert!(skipping.find(&haystack).unwrap().is_none());
        let exhaustive = crate::RegexBuilder::new(r"#(\d)\1")
            .backtrack_limit(1_000_000)
            .build()
            .unwrap();
        assert!(matches!(
            exhaustive.find(&haystack),
            Err(crate::Error::RuntimeError(
                crate::RuntimeError::BacktrackLimitExceeded
            ))
        ));
    }
}
