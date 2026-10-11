// Copyright 2026 Notes Supply AB. Distributed under the MIT license.
//! Expressions restricted to the regular subset of the syntax, searched by
//! regex-automata's automata and PikeVM only.

use alloc::borrow::Cow;
use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::Arc;
use core::convert::Infallible;
use core::fmt;
use core::str::FromStr;

use crate::compile::{CompileOptions, DelegateUsage, compile_inner, compile_inner_from_hir};
use crate::parse::NamedGroups;
use crate::{
    Assertion, BytesMode, CaptureNames, Captures, CapturesImpl, CompileError, Error, Expr, Input,
    IterState, Regex, RegexImpl, RegexInput, RegexOptions, Replacer, Result, WrapEngine,
    capture_names_of, plan_captures_into, plan_find, plan_is_match, ra_input, replacen_with,
    to_hir,
};

/// A compiled expression from the regular subset of the syntax, searched
/// without backtracking.
///
/// It uses the same parser, flags, [`BytesMode`], [`RegexInput`] ranges and
/// anchoring, leftmost-first match semantics and capture numbering as
/// [`Regex`], and reports the same matches, spans and captures for every
/// pattern it accepts, with one exception: outside ASCII mode, on a haystack
/// that is not valid UTF-8, `Regex` runs word boundaries on its backtracking
/// engine, which advances over an invalid byte by the length its leading bits
/// imply. `RegularRegex` tests every position instead, as regex-automata and
/// the `regex` crate do, so empty word-boundary matches next to invalid bytes
/// can differ. Searches cannot fail, so they return plain values.
///
/// # Accepted syntax
///
/// Literals, `.`, character classes, alternation, greedy and lazy repetition,
/// capture groups (numbered and named), inline flags, and the assertions `^`,
/// `$`, `\A`, `\z`, `\b`, `\B`, `\<`, `\>` and the half word boundaries.
///
/// The constructor rejects, with [`CompileError::NotRegular`] naming the
/// construct, anything that needs the backtracking engine: backreferences,
/// lookaround, atomic groups and possessive quantifiers, conditionals,
/// subroutine calls, `DEFINE` groups, `\K`, `\G`, `\R`, `\Z`, absent operators,
/// backtracking control verbs and Oniguruma's multi-line `^`. They are
/// rejected even where [`Regex`] would currently optimize them away, so the
/// regular-only restriction does not depend on optimizer routing or on which
/// Cargo features are enabled. As with [`Regex`], Unicode character classes
/// still require the corresponding Unicode features.
///
/// Builder options that change matching semantics only the backtracking engine
/// implements (`find_not_empty`, `disallow_empty_match_at_eof_after_newline`,
/// `allow_input_assertion_overrides`) are rejected the same way. Options that
/// only tune the backtracking engine (`backtrack_limit`, `seek`, `native`)
/// have no effect: a regular search never backtracks.
///
/// # Complexity
///
/// The engine is regex-automata's meta regex with its bounded backtracker
/// disabled, leaving DFAs, a one-pass DFA and the PikeVM. One search
/// (`is_match`, `find`, `captures`) takes `O(m * n)` time for a compiled
/// pattern of size `m` and a haystack of length `n`. Iterators run one search
/// per match from the end of the previous one, so a complete iteration can take
/// `O(m * n^2)` in the worst case, as in the `regex` crate; no linear bound is
/// claimed for whole iterations.
///
/// # Example
///
/// ```
/// use regexrs::{CompileError, Error, RegularRegex};
///
/// let date = RegularRegex::new(r"(?<year>\d{4})-(?<month>\d{2})")?;
/// let caps = date.captures("due 2026-10, paid 2026-11").expect("a date");
/// assert_eq!(&caps["year"], "2026");
/// let months: Vec<&str> = date.find_iter("2026-10 2026-11").map(|m| m.as_str()).collect();
/// assert_eq!(months, ["2026-10", "2026-11"]);
///
/// assert!(matches!(
///     RegularRegex::new(r"(\w+) \1"),
///     Err(Error::CompileError(e)) if matches!(*e, CompileError::NotRegular("backreference"))
/// ));
/// # Ok::<(), Error>(())
/// ```
#[derive(Clone)]
pub struct RegularRegex {
    /// The meta engine, or a linear plan, which never backtracks either.
    engine: WrapEngine,
    pattern: String,
    /// The pattern in regex-automata syntax with word boundaries removed. A
    /// `RegexSet` member converted from this expression uses it to find
    /// candidate positions, which only needs to over-approximate.
    set_pattern: String,
    named_groups: Arc<NamedGroups>,
}

/// An iterator over the non-overlapping matches of a [`RegularRegex`].
///
/// Created by [`RegularRegex::find_iter`]; follows the same empty-match rules
/// as [`Regex::find_iter`].
#[derive(Debug)]
pub struct RegularMatches<'r, 't, S: Input + ?Sized> {
    regex: &'r RegularRegex,
    state: IterState<'t, S>,
}

/// An iterator over the captures of successive non-overlapping matches of a
/// [`RegularRegex`]. Created by [`RegularRegex::captures_iter`].
#[derive(Debug)]
pub struct RegularCaptureMatches<'r, 't, S: Input + ?Sized>(RegularMatches<'r, 't, S>);

impl RegularRegex {
    /// Compile `pattern` with default options.
    ///
    /// # Errors
    ///
    /// Returns a parse error for invalid syntax, [`CompileError::NotRegular`]
    /// for a construct outside the regular subset, or another compile error,
    /// such as an exceeded size limit.
    pub fn new(pattern: &str) -> Result<RegularRegex> {
        Self::new_options(pattern.into(), &RegexOptions::default())
    }

    pub(crate) fn new_options(pattern: String, options: &RegexOptions) -> Result<RegularRegex> {
        if let Some(option) = rich_option(options) {
            return Err(CompileError::NotRegular(option).into());
        }
        let tree = Expr::parse_tree_with_flags(&pattern, options.compute_flags())?;
        if let Some(construct) = rich_construct(&tree.expr) {
            return Err(CompileError::NotRegular(construct).into());
        }
        let compile_options = CompileOptions {
            bytes_mode: options.bytes_mode,
            unicode: options.syntaxc.get_unicode()
                && !matches!(options.bytes_mode, BytesMode::Ascii),
            delegate_size_limit: options.delegate_size_limit,
            delegate_dfa_size_limit: options.delegate_dfa_size_limit,
            ..CompileOptions::default()
        };
        let set_pattern = boundary_free_pattern(&tree.expr);
        let utf8 = matches!(options.bytes_mode, BytesMode::Unicode);
        let mut hir_ctx = to_hir::HirCtx::new(compile_options.unicode, utf8);
        let usage = DelegateUsage::regular_only();
        let hir = to_hir::expr_to_hir(&tree.expr, &mut hir_ctx);
        let engine = WrapEngine::new(hir.as_ref(), &compile_options, usage, || match &hir {
            Some(hir) => compile_inner_from_hir(hir, &compile_options, usage),
            // Translation only fails for input the engine rejects, whose own
            // error is the useful one. Word boundaries have no faithful string
            // form, so they never take this path.
            None => {
                let inner = compile_inner(&set_pattern, &compile_options, usage)?;
                if set_pattern_differs(&tree.expr) {
                    return Err(CompileError::UnexpectedGeneralError(
                        "word boundaries could not be translated for RegularRegex".into(),
                    )
                    .into());
                }
                Ok(inner)
            }
        })?;
        Ok(RegularRegex {
            engine,
            pattern,
            set_pattern,
            named_groups: Arc::new(tree.named_groups),
        })
    }

    /// The original pattern.
    pub fn as_str(&self) -> &str {
        &self.pattern
    }

    /// Whether the expression matches anywhere in `haystack`.
    pub fn is_match<S: Input + ?Sized>(&self, haystack: &S) -> bool {
        self.is_match_input(RegexInput::new(haystack))
    }

    /// Whether the expression matches within a ranged or anchored input.
    pub fn is_match_input<S: Input + ?Sized>(&self, input: RegexInput<'_, S>) -> bool {
        !input.is_done()
            && match &self.engine {
                WrapEngine::Plan(plan) => plan_is_match(plan, &input),
                WrapEngine::Meta(inner) => inner.is_match(ra_input(&input)),
            }
    }

    /// The leftmost-first match in `haystack`.
    pub fn find<'t, S: Input + ?Sized>(&self, haystack: &'t S) -> Option<S::Match<'t>> {
        self.find_input(RegexInput::new(haystack))
    }

    /// The leftmost-first match within a ranged or anchored input; offsets are
    /// absolute in the original haystack.
    pub fn find_input<'t, S: Input + ?Sized>(
        &self,
        input: RegexInput<'t, S>,
    ) -> Option<S::Match<'t>> {
        self.find_span(&input)
            .map(|(start, end)| input.haystack().make_match(start, end))
    }

    /// Successive non-overlapping matches in `haystack`.
    pub fn find_iter<'r, 't, S: Input + ?Sized>(
        &'r self,
        haystack: &'t S,
    ) -> RegularMatches<'r, 't, S> {
        self.find_iter_input(RegexInput::new(haystack))
    }

    /// Successive non-overlapping matches within a ranged or anchored input.
    pub fn find_iter_input<'r, 't, S: Input + ?Sized>(
        &'r self,
        input: RegexInput<'t, S>,
    ) -> RegularMatches<'r, 't, S> {
        RegularMatches {
            regex: self,
            state: IterState::new(input),
        }
    }

    /// The capture groups of the leftmost-first match in `haystack`.
    pub fn captures<'t, S: Input + ?Sized>(&self, haystack: &'t S) -> Option<Captures<'t, S>> {
        self.captures_input(RegexInput::new(haystack))
    }

    /// The capture groups of the leftmost-first match within a ranged or
    /// anchored input.
    pub fn captures_input<'t, S: Input + ?Sized>(
        &self,
        input: RegexInput<'t, S>,
    ) -> Option<Captures<'t, S>> {
        if input.is_done() {
            return None;
        }
        let mut locations = self.engine.create_captures();
        match &self.engine {
            WrapEngine::Plan(plan) => plan_captures_into(plan, &input, &mut locations),
            WrapEngine::Meta(inner) => inner.search_captures(&ra_input(&input), &mut locations),
        }
        locations.is_match().then(|| Captures {
            inner: CapturesImpl::Wrap {
                locations,
                explicit_capture_group_0: false,
            },
            named_groups: Arc::clone(&self.named_groups),
            input: input.haystack(),
        })
    }

    /// The captures of successive non-overlapping matches in `haystack`.
    pub fn captures_iter<'r, 't, S: Input + ?Sized>(
        &'r self,
        haystack: &'t S,
    ) -> RegularCaptureMatches<'r, 't, S> {
        self.captures_iter_input(RegexInput::new(haystack))
    }

    /// The captures of successive matches within a ranged or anchored input.
    pub fn captures_iter_input<'r, 't, S: Input + ?Sized>(
        &'r self,
        input: RegexInput<'t, S>,
    ) -> RegularCaptureMatches<'r, 't, S> {
        RegularCaptureMatches(self.find_iter_input(input))
    }

    /// The number of capture groups, including the implicit group 0.
    pub fn captures_len(&self) -> usize {
        self.engine.captures_len()
    }

    /// The names of the capture groups, in group order; `None` for unnamed
    /// groups, including group 0.
    pub fn capture_names(&self) -> CaptureNames<'_> {
        capture_names_of(&self.named_groups, self.captures_len())
    }

    /// Replace the leftmost-first match; see [`Regex::replace`] for the
    /// replacement syntax.
    pub fn replace<'t, R: Replacer>(&self, text: &'t str, rep: R) -> Cow<'t, str> {
        self.replacen(text, 1, rep)
    }

    /// Replace all non-overlapping matches.
    pub fn replace_all<'t, R: Replacer>(&self, text: &'t str, rep: R) -> Cow<'t, str> {
        self.replacen(text, 0, rep)
    }

    /// Replace at most `limit` non-overlapping matches, or all of them when
    /// `limit` is 0.
    pub fn replacen<'t, R: Replacer>(&self, text: &'t str, limit: usize, rep: R) -> Cow<'t, str> {
        let replaced = replacen_with(
            text,
            limit,
            rep,
            || self.find_iter(text).map(Ok::<_, Infallible>),
            || self.captures_iter(text).map(Ok),
        );
        match replaced {
            Ok(replaced) => replaced,
            Err(never) => match never {},
        }
    }

    fn find_span<S: Input + ?Sized>(&self, input: &RegexInput<'_, S>) -> Option<(usize, usize)> {
        if input.is_done() {
            return None;
        }
        match &self.engine {
            WrapEngine::Plan(plan) => plan_find(plan, input, None),
            WrapEngine::Meta(inner) => inner
                .search(&ra_input(input))
                .map(|found| (found.start(), found.end())),
        }
    }
}

impl<'r, 't, S: Input + ?Sized> Iterator for RegularMatches<'r, 't, S> {
    type Item = S::Match<'t>;

    fn next(&mut self) -> Option<S::Match<'t>> {
        let RegularMatches { regex, state } = self;
        let haystack = state.input.haystack();
        let found = state.next_with(|input, _| {
            Ok::<_, Infallible>(
                regex
                    .find_span(input)
                    .map(|(start, end)| (haystack.make_match(start, end), (start, end))),
            )
        })?;
        match found {
            Ok(found) => Some(found),
            Err(never) => match never {},
        }
    }
}

impl<'r, 't, S: Input + ?Sized> Iterator for RegularCaptureMatches<'r, 't, S> {
    type Item = Captures<'t, S>;

    fn next(&mut self) -> Option<Captures<'t, S>> {
        let RegularMatches { regex, state } = &mut self.0;
        let found = state.next_with(|input, _| {
            Ok::<_, Infallible>(regex.captures_input(input.clone()).map(|captures| {
                let span = captures.get_span(0).expect("a match always has group 0");
                (captures, span)
            }))
        })?;
        match found {
            Ok(found) => Some(found),
            Err(never) => match never {},
        }
    }
}

impl From<RegularRegex> for Regex {
    /// Share the compiled engine with a [`Regex`], for APIs such as
    /// [`Regex::searcher`] or [`crate::RegexSet::from_regexes`]. The result
    /// matches exactly as the regular expression did.
    fn from(regex: RegularRegex) -> Regex {
        Regex {
            inner: RegexImpl::Wrap {
                engine: regex.engine,
                pattern: regex.pattern,
                explicit_capture_group_0: false,
                delegated_pattern: regex.set_pattern,
            },
            named_groups: regex.named_groups,
            #[cfg(all(feature = "native", target_os = "linux", target_arch = "x86_64"))]
            native: None,
        }
    }
}

impl fmt::Debug for RegularRegex {
    /// Shows the original pattern.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.pattern)
    }
}

impl fmt::Display for RegularRegex {
    /// Shows the original pattern.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.pattern)
    }
}

impl FromStr for RegularRegex {
    type Err = Error;

    fn from_str(pattern: &str) -> Result<RegularRegex> {
        RegularRegex::new(pattern)
    }
}

/// The first rich-only option set in `options`.
pub(crate) fn rich_option(options: &RegexOptions) -> Option<&'static str> {
    let runtime = &options.hard_regex_runtime_options;
    if runtime.find_not_empty {
        Some("the find_not_empty option")
    } else if runtime.disallow_empty_match_at_eof_after_newline {
        Some("the disallow_empty_match_at_eof_after_newline option")
    } else if runtime.allow_input_assertion_overrides {
        Some("the allow_input_assertion_overrides option")
    } else {
        None
    }
}

/// The first construct outside the regular subset, in pattern order. Every
/// variant is listed so a new construct must be classified explicitly.
pub(crate) fn rich_construct(expr: &Expr) -> Option<&'static str> {
    match expr {
        Expr::Empty | Expr::Any { .. } | Expr::Literal { .. } | Expr::Delegate { .. } => None,
        Expr::Assertion(Assertion::EndTextIgnoreTrailingNewlines { .. }) => {
            Some(r"`\Z` (end before trailing newlines)")
        }
        Expr::Assertion(Assertion::StartLineOniguruma { .. }) => Some("Oniguruma's multi-line `^`"),
        Expr::Assertion(_) => None,
        Expr::Concat(children) | Expr::Alt(children) => children.iter().find_map(rich_construct),
        Expr::Group(child) => rich_construct(child),
        Expr::Repeat { child, .. } => rich_construct(child),
        Expr::LookAround(..) => Some("lookaround"),
        Expr::Backref { .. } | Expr::BackrefWithRelativeRecursionLevel { .. } => {
            Some("backreference")
        }
        Expr::AtomicGroup(_) => Some("atomic group or possessive quantifier"),
        Expr::KeepOut => Some(r"`\K`"),
        Expr::ContinueFromPreviousMatchEnd => Some(r"`\G`"),
        Expr::BackrefExistsCondition { .. } | Expr::Conditional { .. } => Some("conditional"),
        Expr::SubroutineCall(_) => Some("subroutine call"),
        Expr::BacktrackingControlVerb(_) => Some("backtracking control verb"),
        Expr::Absent(_) => Some("absent operator"),
        Expr::DefineGroup { .. } => Some("DEFINE group"),
        Expr::GeneralNewline { .. } => Some(r"`\R` (general newline)"),
        Expr::AstNode(..) => Some("unresolved group reference"),
    }
}

fn is_word_boundary(expr: &Expr) -> bool {
    matches!(
        expr,
        Expr::Assertion(
            Assertion::WordBoundary
                | Assertion::NotWordBoundary
                | Assertion::LeftWordBoundary
                | Assertion::RightWordBoundary
                | Assertion::LeftWordHalfBoundary
                | Assertion::RightWordHalfBoundary
        )
    )
}

/// Whether the tree contains a word boundary, which the string form drops.
fn set_pattern_differs(expr: &Expr) -> bool {
    is_word_boundary(expr) || expr.children_iter().any(set_pattern_differs)
}

/// The regex-automata syntax for an accepted tree, with word boundaries
/// removed: `Expr::to_str` has no form for them that keeps the VM's Unicode
/// semantics in every bytes mode, and dropping a zero-width assertion only
/// widens the language.
fn boundary_free_pattern(expr: &Expr) -> String {
    let mut pattern = String::new();
    if set_pattern_differs(expr) {
        without_word_boundaries(expr).to_str(&mut pattern, 0);
    } else {
        expr.to_str(&mut pattern, 0);
    }
    pattern
}

fn without_word_boundaries(expr: &Expr) -> Expr {
    match expr {
        _ if is_word_boundary(expr) => Expr::Empty,
        Expr::Concat(children) => {
            Expr::Concat(children.iter().map(without_word_boundaries).collect())
        }
        Expr::Alt(children) => Expr::Alt(children.iter().map(without_word_boundaries).collect()),
        Expr::Group(child) => Expr::Group(Arc::new(without_word_boundaries(child))),
        Expr::Repeat {
            child,
            lo,
            hi,
            greedy,
        } => match without_word_boundaries(child) {
            // A repeated empty expression is empty; `to_str` would otherwise
            // attach the quantifier to whatever precedes it.
            Expr::Empty => Expr::Empty,
            child => Expr::Repeat {
                child: Box::new(child),
                lo: *lo,
                hi: *hi,
                greedy: *greedy,
            },
        },
        // Accepted trees contain no other interior nodes.
        leaf => leaf.clone(),
    }
}
