// Copyright 2026 Notes Supply AB. Distributed under the MIT license.
//! Caller-owned search working memory. All selection logic stays in `Regex`.
use crate::{Captures, Input, Regex, RegexImpl, RegexInput, Result, vm};
use alloc::boxed::Box;
use regex_automata::{meta, util::captures::Captures as RaCaptures};

pub(crate) enum Cache {
    Regular {
        cache: Box<meta::Cache>,
        captures: RaCaptures,
    },
    Rich(vm::Scratch),
}

impl Cache {
    pub(crate) fn regular(&mut self) -> Option<(&mut meta::Cache, &mut RaCaptures)> {
        match self {
            Self::Regular { cache, captures } => Some((cache.as_mut(), captures)),
            Self::Rich(_) => None,
        }
    }

    pub(crate) fn rich(&mut self) -> Option<&mut vm::Scratch> {
        match self {
            Self::Rich(scratch) => Some(scratch),
            Self::Regular { .. } => None,
        }
    }
}

/// Reusable scratch for repeated searches with one immutable [`Regex`].
///
/// No haystack or result is retained between searches. The mutable borrow
/// keeps one worker's scratch exclusive; each worker can create its own searcher.
/// Owned [`Captures`] still allocate a result snapshot so later searches cannot
/// change an earlier result. The ordinary `Regex` methods remain thread-safe.
pub struct Searcher<'r> {
    regex: &'r Regex,
    cache: Cache,
}

impl core::fmt::Debug for Searcher<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Searcher")
            .field("regex", &self.regex)
            .finish_non_exhaustive()
    }
}

impl<'r> Searcher<'r> {
    pub(crate) fn new(regex: &'r Regex) -> Self {
        let cache = match &regex.inner {
            RegexImpl::Wrap { inner, .. } => Cache::Regular {
                cache: Box::new(inner.create_cache()),
                captures: inner.create_captures(),
            },
            RegexImpl::Fancy { .. } => Cache::Rich(vm::new_scratch()),
        };
        Self { regex, cache }
    }

    /// The immutable expression associated with this searcher's scratch.
    pub fn regex(&self) -> &'r Regex {
        self.regex
    }

    /// Test whether any match exists, reusing working memory.
    pub fn is_match<S: Input + ?Sized>(&mut self, input: &S) -> Result<bool> {
        if let Some(found) = self.regex.native_match(input) {
            return Ok(found);
        }
        self.is_match_input(RegexInput::new(input))
    }

    /// Test a ranged or anchored search input with the original matching rules.
    pub fn is_match_input<S: Input + ?Sized>(&mut self, input: RegexInput<'_, S>) -> Result<bool> {
        if input.is_done() {
            return Ok(false);
        }
        if let (RegexImpl::Wrap { inner, .. }, Some((cache, _))) =
            (&self.regex.inner, self.cache.regular())
        {
            return Ok(inner
                .search_half_with(cache, &crate::ra_input(&input).earliest(true))
                .is_some());
        }
        Ok(self
            .regex
            .find_input_raw_cached(&input, 0, Some(&mut self.cache))?
            .is_some())
    }

    /// Find the leftmost-first match without checking out pooled scratch.
    pub fn find<'t, S: Input + ?Sized>(&mut self, input: &'t S) -> Result<Option<S::Match<'t>>> {
        self.find_input(RegexInput::new(input))
    }

    /// Find within a ranged or anchored input, preserving assertion context.
    pub fn find_input<'t, S: Input + ?Sized>(
        &mut self,
        input: RegexInput<'t, S>,
    ) -> Result<Option<S::Match<'t>>> {
        Ok(self
            .regex
            .find_input_raw_cached(&input, 0, Some(&mut self.cache))?
            .map(|(start, end)| input.haystack().make_match(start, end)))
    }

    /// Return owned captures; later searches do not overwrite these results.
    pub fn captures<'t, S: Input + ?Sized>(
        &mut self,
        input: &'t S,
    ) -> Result<Option<Captures<'t, S>>> {
        self.captures_input(RegexInput::new(input))
    }

    /// Capture within a ranged or anchored input, preserving original errors.
    pub fn captures_input<'t, S: Input + ?Sized>(
        &mut self,
        input: RegexInput<'t, S>,
    ) -> Result<Option<Captures<'t, S>>> {
        self.regex
            .captures_input_cached(&input, 0, Some(&mut self.cache))
    }
}
