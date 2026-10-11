// Copyright 2026 Notes Supply AB. Distributed under the MIT license.
//! Automata for patterns that need the backtracking VM only for word
//! boundaries. `Regex::regular_route` decides per search whether their
//! results provably equal the VM's.
//!
//! The automata are built on the first search that can use them, so
//! constructing a `Regex` stays as cheap as compiling its VM program.
//!
//! A pattern whose matches follow one deterministic path is searched by its
//! linear plan instead, which needs neither automaton. The plan only answers
//! where the lazy DFAs would also have answered, so whatever the VM would
//! have reported elsewhere, including its resource errors, is unchanged.
//!
//! Otherwise spans come from regex-automata's lazy DFAs, configured as its
//! meta engine configures them. With Unicode word boundaries they give up on reaching a
//! non-ASCII byte they would have to classify. The caller then runs the VM,
//! which is faster on such text than the NFA engines the meta engine would
//! fall back to. Captures within a span the DFAs found come from the meta
//! engine, which then only searches that span; it is built for the first
//! search that needs it.

use alloc::boxed::Box;
use alloc::sync::Arc;
use core::panic::{RefUnwindSafe, UnwindSafe};
use regex_automata::hybrid;
use regex_automata::meta::Regex as RaRegex;
use regex_automata::nfa::thompson::{self, WhichCaptures};
use regex_automata::util::captures::Captures as RaCaptures;
use regex_automata::util::lazy::Lazy;
use regex_automata::util::look::LookMatcher;
use regex_automata::util::pool::Pool;
use regex_automata::util::prefilter::Prefilter;
use regex_automata::{Input, MatchError, MatchKind};
use regex_syntax::hir::{Hir, Look};

use crate::compile::{self, CompileOptions, DelegateUsage};
use crate::plan::{LinearPlan, NFA_UNIT_BUDGET, PlanInput, Slots};

type BuildFn = Box<dyn Fn() -> Option<Automata> + Send + Sync + UnwindSafe + RefUnwindSafe>;
type MetaFn = Box<dyn Fn() -> Option<RaRegex> + Send + Sync + UnwindSafe + RefUnwindSafe>;
type CacheFn = Box<dyn Fn() -> hybrid::regex::Cache + Send + Sync + UnwindSafe + RefUnwindSafe>;
type EngineFn = Box<dyn Fn() -> RouteEngine + Send + Sync + UnwindSafe + RefUnwindSafe>;

pub(crate) struct RegularRoute {
    /// ASCII bytes mode: the VM steps one byte at a time, as the automata do.
    pub(crate) ascii: bool,
    engine: Lazy<RouteEngine, EngineFn>,
}

/// How a route searches, decided on its first search.
pub(crate) enum RouteEngine {
    /// The pattern's linear plan, and the automata for the searches it leaves
    /// to them, built on the first such search.
    Plan(RoutePlan, Lazy<Option<Automata>, BuildFn>),
    /// The automata, or `None` if they do not fit within the configured
    /// limits.
    Automata(Option<Automata>),
}

pub(crate) struct RoutePlan {
    plan: LinearPlan,
    /// The automata give up at a non-ASCII byte: Unicode word boundaries.
    ascii_only: bool,
}

impl RegularRoute {
    /// A route for `hir`, whose automata will be compiled with `options`.
    pub(crate) fn new(
        hir: Hir,
        options: CompileOptions,
        usage: DelegateUsage,
        ascii: bool,
    ) -> Self {
        let hir = Arc::new(hir);
        let engine: EngineFn = Box::new(move || {
            let (hir, options) = (Arc::clone(&hir), options.clone());
            // Explicit size limits can keep the automata from being built,
            // which leaves every search to the VM; a plan must not answer
            // instead.
            let default_limits =
                options.delegate_size_limit.is_none() && options.delegate_dfa_size_limit.is_none();
            let plan = default_limits
                .then(|| RoutePlan::new(&hir, usage.prefilter()))
                .flatten();
            let build: BuildFn = Box::new(move || Automata::new(&hir, &options, usage));
            match plan {
                Some(plan) => RouteEngine::Plan(plan, Lazy::new(build)),
                None => RouteEngine::Automata(build()),
            }
        });
        Self {
            ascii,
            engine: Lazy::new(engine),
        }
    }

    /// The route's engine, decided and built on first use.
    #[inline]
    pub(crate) fn engine(&self) -> &RouteEngine {
        Lazy::get(&self.engine)
    }
}

impl RouteEngine {
    #[inline]
    pub(crate) fn plan(&self) -> Option<&RoutePlan> {
        match self {
            RouteEngine::Plan(plan, _) => Some(plan),
            RouteEngine::Automata(_) => None,
        }
    }

    /// The automata, built on first use; `None` if they do not fit within
    /// the configured limits.
    #[inline]
    pub(crate) fn automata(&self) -> Option<&Automata> {
        match self {
            RouteEngine::Plan(_, automata) => Lazy::get(automata).as_ref(),
            RouteEngine::Automata(automata) => automata.as_ref(),
        }
    }
}

impl RoutePlan {
    /// The plan for `hir`, when the automata it stands in for certainly fit
    /// within the default limits.
    fn new(hir: &Hir, prefilter: bool) -> Option<RoutePlan> {
        let plan = LinearPlan::new(hir, prefilter)?;
        (plan.nfa_units() <= NFA_UNIT_BUDGET).then(|| RoutePlan {
            plan,
            ascii_only: hir.properties().look_set().contains_word_unicode(),
        })
    }

    /// Whether the automata, searching from `input.start` and reading up to
    /// `reach`, would have answered rather than given up. With Unicode word
    /// boundaries they read the byte before the start and stop at the first
    /// non-ASCII byte; one byte of margin past `reach` is checked as well.
    fn automata_answer(&self, input: &PlanInput<'_>, reach: usize) -> bool {
        let haystack = input.haystack;
        !self.ascii_only
            || haystack[input.start.saturating_sub(1)..(reach + 2).min(haystack.len())].is_ascii()
    }

    /// The leftmost-first span, filling `slots` when given, or `None` where
    /// the automata could have given up.
    pub(crate) fn find<S: Slots + ?Sized>(
        &self,
        input: &PlanInput<'_>,
        slots: Option<&mut S>,
    ) -> Option<Option<(usize, usize)>> {
        // Any match ends at least a byte past the start, so the checked
        // span always reaches three bytes past it: text that makes the
        // automata give up there needs no plan search.
        let haystack = input.haystack;
        let near = &haystack[input.start.saturating_sub(1)..(input.start + 3).min(haystack.len())];
        if self.ascii_only && !near.is_ascii() {
            return None;
        }
        let found = self.plan.find(input, slots);
        let reach = found.map_or(haystack.len(), |(_, end)| end);
        self.automata_answer(input, reach).then_some(found)
    }

    /// Whether there is a match, or `None` where the automata could have
    /// given up.
    pub(crate) fn is_match(&self, input: &PlanInput<'_>) -> Option<bool> {
        // A membership test may stop before the match ends, but the span
        // bounds what the automata read either way.
        self.find::<[usize]>(input, None)
            .map(|found| found.is_some())
    }

    /// Captures storage for a plan search, or `None` where the automata could
    /// have given up.
    pub(crate) fn captures(&self, input: &PlanInput<'_>) -> Option<Option<RaCaptures>> {
        let mut locations = self.plan.create_captures();
        let found = self.find(input, Some(locations.slots_mut()))?;
        locations.set_pattern(found.map(|_| regex_automata::PatternID::ZERO));
        Some(found.map(|_| locations))
    }

    /// The number of slots, two per group.
    pub(crate) fn slot_len(&self) -> usize {
        self.plan.slot_len()
    }
}

pub(crate) struct Automata {
    dfa: Arc<hybrid::regex::Regex>,
    caches: Pool<hybrid::regex::Cache, CacheFn>,
    /// The same pattern's meta engine, for captures.
    regex: Lazy<Option<RaRegex>, MetaFn>,
}

impl Automata {
    /// Lazy DFAs for `hir`, configured like the meta engine `options` would
    /// build, which is itself only built when needed.
    fn new(hir: &Hir, options: &CompileOptions, usage: DelegateUsage) -> Option<Self> {
        let config = compile::meta_config(options, usage);
        let mut look_matcher = LookMatcher::new();
        look_matcher.set_line_terminator(config.get_line_terminator());
        let thompson = thompson::Config::new()
            .utf8(config.get_utf8_empty())
            .nfa_size_limit(config.get_nfa_size_limit())
            .shrink(false)
            .which_captures(WhichCaptures::None)
            .look_matcher(look_matcher);
        let forward = thompson::Compiler::new()
            .configure(thompson.clone())
            .build_from_hir(hir)
            .ok()?;
        let reverse = thompson::Compiler::new()
            .configure(thompson.reverse(true))
            .build_from_hir(hir)
            .ok()?;
        let prefilter = if config.get_auto_prefilter()
            && !hir.properties().look_set_prefix().contains(Look::Start)
        {
            Prefilter::from_hir_prefix(MatchKind::LeftmostFirst, hir)
        } else {
            None
        };
        let dfa = hybrid::dfa::Config::new()
            .match_kind(MatchKind::LeftmostFirst)
            .specialize_start_states(prefilter.is_some())
            .prefilter(prefilter)
            .starts_for_each_pattern(true)
            .byte_classes(config.get_byte_classes())
            .unicode_word_boundary(true)
            .cache_capacity(config.get_hybrid_cache_capacity())
            .skip_cache_capacity_check(false)
            .minimum_cache_clear_count(Some(3))
            .minimum_bytes_per_state(Some(10));
        let forward = hybrid::dfa::Builder::new()
            .configure(dfa.clone())
            .build_from_nfa(forward)
            .ok()?;
        let reverse = hybrid::dfa::Builder::new()
            .configure(
                dfa.match_kind(MatchKind::All)
                    .prefilter(None)
                    .specialize_start_states(false),
            )
            .build_from_nfa(reverse)
            .ok()?;
        let dfa = Arc::new(hybrid::regex::Builder::new().build_from_dfas(forward, reverse));
        let create = Arc::clone(&dfa);
        let create: CacheFn = Box::new(move || create.create_cache());
        let (hir, options) = (hir.clone(), options.clone());
        let regex: MetaFn =
            Box::new(move || compile::compile_inner_from_hir(&hir, &options, usage).ok());
        Some(Self {
            caches: Pool::new(create),
            dfa,
            regex: Lazy::new(regex),
        })
    }

    /// The meta engine, built on first use; `None` if it does not fit within
    /// the configured limits.
    pub(crate) fn regex(&self) -> Option<&RaRegex> {
        Lazy::get(&self.regex).as_ref()
    }

    pub(crate) fn create_cache(&self) -> hybrid::regex::Cache {
        self.dfa.create_cache()
    }

    /// The leftmost-first match span, or `Err` where the DFAs gave up.
    pub(crate) fn find(
        &self,
        input: &Input<'_>,
        cache: Option<&mut hybrid::regex::Cache>,
    ) -> Result<Option<(usize, usize)>, MatchError> {
        let found = match cache {
            Some(cache) => self.dfa.try_search(cache, input),
            None => self.dfa.try_search(&mut self.caches.get(), input),
        }?;
        Ok(found.map(|found| (found.start(), found.end())))
    }

    /// Whether there is a match, or `Err` where the DFA gave up.
    pub(crate) fn is_match(
        &self,
        input: &Input<'_>,
        cache: Option<&mut hybrid::regex::Cache>,
    ) -> Result<bool, MatchError> {
        let input = input.clone().earliest(true);
        let found = match cache {
            Some(cache) => self
                .dfa
                .forward()
                .try_search_fwd(cache.forward_mut(), &input),
            None => {
                let mut cache = self.caches.get();
                self.dfa
                    .forward()
                    .try_search_fwd(cache.forward_mut(), &input)
            }
        }?;
        Ok(found.is_some())
    }
}
