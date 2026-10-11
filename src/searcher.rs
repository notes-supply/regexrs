// Copyright 2026 Notes Supply AB. Distributed under the MIT license.
//! Caller-owned search working memory. All selection logic stays in `Regex`.
use crate::regular_route::Automata;
use crate::{
    CaptureLocations, CaptureMatches, Captures, Input, Matches, Regex, RegexImpl, RegexInput,
    Result, vm,
};
use alloc::boxed::Box;
use regex_automata::{hybrid, meta, util::captures::Captures as RaCaptures};

pub(crate) enum Cache {
    Regular {
        cache: Box<meta::Cache>,
        captures: RaCaptures,
    },
    /// VM scratch, plus the caches and capture storage of a pattern's
    /// regular route, created when it is first used.
    Rich {
        scratch: vm::Scratch,
        route: Option<RouteCache>,
    },
}

pub(crate) struct RouteCache {
    dfa: Box<hybrid::regex::Cache>,
    /// The meta engine's cache and capture storage, created for the first
    /// search that uses that engine.
    meta: Option<(Box<meta::Cache>, RaCaptures)>,
}

impl core::fmt::Debug for Cache {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            Self::Regular { .. } => "Cache::Regular",
            Self::Rich { .. } => "Cache::Rich",
        })
    }
}

impl Cache {
    pub(crate) fn regular(&mut self) -> Option<(&mut meta::Cache, &mut RaCaptures)> {
        match self {
            Self::Regular { cache, captures } => Some((cache.as_mut(), captures)),
            Self::Rich { .. } => None,
        }
    }

    /// The lazy DFAs' cache of a pattern's regular route, after creating
    /// the route's caches if this is its first use.
    pub(crate) fn route(&mut self, automata: &Automata) -> Option<&mut hybrid::regex::Cache> {
        match self {
            Self::Rich { route, .. } => Some(Self::route_cache(route, automata).dfa.as_mut()),
            Self::Regular { .. } => None,
        }
    }

    /// The route's meta engine cache and capture storage, created on first use.
    pub(crate) fn route_meta(
        &mut self,
        automata: &Automata,
    ) -> Option<(&mut meta::Cache, &mut RaCaptures)> {
        let Self::Rich { route, .. } = self else {
            return None;
        };
        let regex = automata.regex()?;
        let (cache, captures) = Self::route_cache(route, automata)
            .meta
            .get_or_insert_with(|| (Box::new(regex.create_cache()), regex.create_captures()));
        Some((cache.as_mut(), captures))
    }

    fn route_cache<'c>(
        route: &'c mut Option<RouteCache>,
        automata: &Automata,
    ) -> &'c mut RouteCache {
        route.get_or_insert_with(|| RouteCache {
            dfa: Box::new(automata.create_cache()),
            meta: None,
        })
    }

    pub(crate) fn rich(&mut self) -> Option<&mut vm::Scratch> {
        match self {
            Self::Rich { scratch, .. } => Some(scratch),
            Self::Regular { .. } => None,
        }
    }
}

/// Reusable scratch for repeated searches with one immutable [`Regex`].
///
/// No haystack or result is retained between searches. The mutable borrow
/// keeps one worker's scratch exclusive; each worker can create its own searcher.
/// Owned [`Captures`] still allocate a result snapshot so later searches cannot
/// change an earlier result; [`Searcher::captures_read`] instead overwrites
/// caller-owned [`CaptureLocations`]. The ordinary `Regex` methods remain
/// thread-safe.
///
/// Iterators borrow the searcher's scratch until they are dropped and follow
/// the same rules as [`Regex::find_iter`] and [`Regex::captures_iter`]: empty
/// matches immediately after a match are skipped, and iteration ends after the
/// first error.
///
/// ```
/// use regexrs::Regex;
/// let regex = Regex::new(r"(?<word>\w+)\s+\k<word>")?;
/// let mut search = regex.searcher();
/// let mut locations = regex.capture_locations();
/// for line in ["one one", "two three", "four four"] {
///     if search.captures_read(&mut locations, line)?.is_some() {
///         let (start, end) = locations.name("word").expect("participating group");
///         assert_eq!(&line[start..end], line.split(' ').next().unwrap());
///     }
/// }
/// let repeated: Vec<&str> = search
///     .find_iter("a a b c c")
///     .map(|m| m.map(|m| m.as_str()))
///     .collect::<Result<_, _>>()?;
/// assert_eq!(repeated, ["a a", "c c"]);
/// # Ok::<(), regexrs::Error>(())
/// ```
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
            RegexImpl::Fancy { .. } => Cache::Rich {
                scratch: vm::new_scratch(),
                route: None,
            },
        };
        Self { regex, cache }
    }

    /// The immutable expression associated with this searcher's scratch.
    pub fn regex(&self) -> &'r Regex {
        self.regex
    }

    /// Test whether any match exists, reusing working memory.
    pub fn is_match<S: Input + ?Sized>(&mut self, input: &S) -> Result<bool> {
        match &self.regex.inner {
            RegexImpl::Wrap { .. } => self.is_match_input(RegexInput::new(input)),
            RegexImpl::Fancy { .. } => {
                self.regex
                    .fancy_is_match(&RegexInput::new(input), true, Some(&mut self.cache))
            }
        }
    }

    /// Test a ranged or anchored search input with the original matching rules.
    pub fn is_match_input<S: Input + ?Sized>(&mut self, input: RegexInput<'_, S>) -> Result<bool> {
        match &self.regex.inner {
            RegexImpl::Wrap { inner, .. } => {
                if input.is_done() {
                    return Ok(false);
                }
                if let Some((cache, _)) = self.cache.regular() {
                    return Ok(inner
                        .search_half_with(cache, &crate::ra_input(&input).earliest(true))
                        .is_some());
                }
                Ok(self
                    .regex
                    .find_input_raw_cached(&input, 0, Some(&mut self.cache))?
                    .is_some())
            }
            RegexImpl::Fancy { .. } => {
                self.regex
                    .fancy_is_match(&input, false, Some(&mut self.cache))
            }
        }
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

    /// Iterate over successive non-overlapping matches, reusing this
    /// searcher's scratch. See [`Regex::find_iter`].
    pub fn find_iter<'s, 't, S: Input + ?Sized>(
        &'s mut self,
        haystack: &'t S,
    ) -> Matches<'s, 't, S> {
        self.find_iter_input(RegexInput::new(haystack))
    }

    /// Iterate over matches in a ranged or anchored input, reusing this
    /// searcher's scratch. See [`Regex::find_iter_input`].
    pub fn find_iter_input<'s, 't, S: Input + ?Sized>(
        &'s mut self,
        input: RegexInput<'t, S>,
    ) -> Matches<'s, 't, S> {
        Matches::new(self.regex, input, Some(&mut self.cache))
    }

    /// Iterate over owned capture snapshots of successive matches, reusing
    /// this searcher's scratch. See [`Regex::captures_iter`].
    pub fn captures_iter<'s, 't, S: Input + ?Sized>(
        &'s mut self,
        haystack: &'t S,
    ) -> CaptureMatches<'s, 't, S> {
        self.captures_iter_input(RegexInput::new(haystack))
    }

    /// Iterate over capture snapshots in a ranged or anchored input, reusing
    /// this searcher's scratch.
    pub fn captures_iter_input<'s, 't, S: Input + ?Sized>(
        &'s mut self,
        input: RegexInput<'t, S>,
    ) -> CaptureMatches<'s, 't, S> {
        CaptureMatches(self.find_iter_input(input))
    }

    /// Search `haystack`, writing capture spans into caller-owned `locations`
    /// instead of allocating a [`Captures`] snapshot, and return the overall
    /// match.
    ///
    /// On a match, `locations` holds every group's span until the next
    /// search with them. On a miss or an error they are cleared, so no span
    /// from an earlier search survives. Locations created by another regex
    /// are replaced with storage for this one, which allocates once.
    ///
    /// # Errors
    ///
    /// Returns matching errors, such as a backtrack-limit error, as
    /// [`Searcher::captures`] does.
    pub fn captures_read<'t, S: Input + ?Sized>(
        &mut self,
        locations: &mut CaptureLocations,
        haystack: &'t S,
    ) -> Result<Option<S::Match<'t>>> {
        self.captures_read_input(locations, RegexInput::new(haystack))
    }

    /// Like [`Searcher::captures_read`], for a ranged or anchored input.
    pub fn captures_read_input<'t, S: Input + ?Sized>(
        &mut self,
        locations: &mut CaptureLocations,
        input: RegexInput<'t, S>,
    ) -> Result<Option<S::Match<'t>>> {
        let found = self
            .regex
            .captures_read_cached(&input, locations, Some(&mut self.cache))?;
        Ok(found
            .then(|| locations.get(0))
            .flatten()
            .map(|(start, end)| input.haystack().make_match(start, end)))
    }
}
