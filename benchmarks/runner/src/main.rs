//! Rebar BYOB runner for regexrs and current Rust regex engines.
//!
//! `regexrs-rebar-runner ENGINE [--version | --verify]` reads one benchmark
//! as KLV on stdin. Without a flag it times the benchmark's model with Rebar's
//! timer and prints `nanoseconds,count` samples. `--verify` replays the same
//! calls outside any timing and compares the engine's ordered match spans,
//! every capture span and, for line models, each record's boolean result or
//! first-match groups against Fancy Regex as the leftmost-first oracle.
//!
//! Besides Rebar's models, `fresh-MODEL` times the first run of `MODEL` on an
//! instance constructed outside the timed interval and never searched before,
//! including its lazily built engines and caches, and `startup-MODEL` times
//! construction plus that first run. Each sample uses a new instance, dropped
//! outside the interval.

use std::io::Write;

use anyhow::{Context, Result, anyhow, bail};

type Span = (usize, usize);
type CaptureSpans = Vec<Vec<Option<Span>>>;

/// A compiled regex of one engine configuration.
enum Engine {
    /// `regexrs::Regex` with native code disabled: the default build.
    Regexrs(regexrs::Regex),
    /// `regexrs::Regex` with native boolean matching (feature `native`).
    RegexrsNative(regexrs::Regex),
    /// `regexrs::Regex` searched through one reused `Searcher`.
    RegexrsSearcher(regexrs::Regex),
    /// `regexrs::Regex` with its opt-in seek prefilter, native code disabled.
    RegexrsSeek(regexrs::Regex),
    /// `regexrs::RegularRegex`.
    RegexrsRegular(regexrs::RegularRegex),
    Regex(regex::Regex),
    Fancy(fancy_regex::Regex),
    /// Fancy Regex with its opt-in seek prefilter.
    FancySeek(fancy_regex::Regex),
    /// Leftmost-longest; only listed for benchmarks where verification shows
    /// the same spans as leftmost-first engines.
    Resharp(Box<resharp::Regex>),
}

const ENGINES: &str = "regexrs, regexrs-native, regexrs-searcher, regexrs-seek, \
                       regexrs-regular, regex, fancy, fancy-seek, resharp";

fn version(engine: &str) -> Result<&'static str> {
    Ok(match engine {
        // The checked-out source; `mise run bench:rebar:measure` records its commit.
        "regexrs" | "regexrs-searcher" | "regexrs-seek" | "regexrs-regular" | "regexrs-native" => {
            "workspace"
        }
        "regex" => "1.13.1",
        "fancy" | "fancy-seek" => "0.19.2",
        "resharp" => "0.7.5",
        _ => bail!("unknown engine {engine:?}; expected one of {ENGINES}"),
    })
}

fn regexrs_options(
    benchmark: &klv::Benchmark,
    native: bool,
    seek: bool,
) -> regexrs::RegexOptionsBuilder {
    let mut options = regexrs::RegexOptionsBuilder::new();
    options
        .unicode_mode(benchmark.regex.unicode)
        .case_insensitive(benchmark.regex.case_insensitive)
        .native(native)
        .seek(seek);
    options
}

fn fancy(benchmark: &klv::Benchmark, pattern: &str, seek: bool) -> Result<fancy_regex::Regex> {
    Ok(fancy_regex::RegexBuilder::new(pattern)
        .unicode_mode(benchmark.regex.unicode)
        .case_insensitive(benchmark.regex.case_insensitive)
        .seek(seek)
        .build()?)
}

impl Engine {
    fn compile(engine: &str, benchmark: &klv::Benchmark) -> Result<Engine> {
        let [pattern] = benchmark.regex.patterns.as_slice() else {
            bail!("this runner supports exactly one pattern per benchmark");
        };
        let pattern = pattern.as_str();
        Ok(match engine {
            "regexrs" => {
                Engine::Regexrs(regexrs_options(benchmark, false, false).build(pattern.into())?)
            }
            "regexrs-searcher" => Engine::RegexrsSearcher(
                regexrs_options(benchmark, false, false).build(pattern.into())?,
            ),
            "regexrs-seek" => {
                Engine::RegexrsSeek(regexrs_options(benchmark, false, true).build(pattern.into())?)
            }
            "regexrs-regular" => Engine::RegexrsRegular(
                regexrs_options(benchmark, false, false).build_regular(pattern.into())?,
            ),
            "regexrs-native" if cfg!(feature = "native") => Engine::RegexrsNative(
                regexrs_options(benchmark, true, false).build(pattern.into())?,
            ),
            "regexrs-native" => bail!("regexrs-native requires the runner's `native` feature"),
            "regex" => Engine::Regex(
                regex::RegexBuilder::new(pattern)
                    .unicode(benchmark.regex.unicode)
                    .case_insensitive(benchmark.regex.case_insensitive)
                    .build()?,
            ),
            "fancy" => Engine::Fancy(fancy(benchmark, pattern, false)?),
            "fancy-seek" => Engine::FancySeek(fancy(benchmark, pattern, true)?),
            "resharp" => {
                let unicode = if benchmark.regex.unicode {
                    resharp::UnicodeMode::Full
                } else {
                    resharp::UnicodeMode::Ascii
                };
                let options = resharp::RegexOptions::default()
                    .unicode(unicode)
                    .case_insensitive(benchmark.regex.case_insensitive)
                    .multiline(false);
                Engine::Resharp(Box::new(
                    resharp::Regex::with_options(pattern, options).map_err(|e| anyhow!("{e}"))?,
                ))
            }
            _ => {
                version(engine)?;
                bail!("unknown engine {engine:?}")
            }
        })
    }

    fn has_captures(&self) -> bool {
        !matches!(self, Engine::Resharp(_))
    }
}

/// The capture groups of one match: an owned snapshot, or caller-owned
/// locations an engine filled in place.
trait Groups {
    fn len(&self) -> usize;
    fn span(&self, i: usize) -> Option<Span>;
}

impl Groups for regexrs::Captures<'_> {
    fn len(&self) -> usize {
        regexrs::Captures::len(self)
    }
    fn span(&self, i: usize) -> Option<Span> {
        self.get(i).map(|m| (m.start(), m.end()))
    }
}

impl Groups for regexrs::CaptureLocations {
    fn len(&self) -> usize {
        regexrs::CaptureLocations::len(self)
    }
    fn span(&self, i: usize) -> Option<Span> {
        self.get(i)
    }
}

impl Groups for regex::Captures<'_> {
    fn len(&self) -> usize {
        regex::Captures::len(self)
    }
    fn span(&self, i: usize) -> Option<Span> {
        self.get(i).map(|m| (m.start(), m.end()))
    }
}

impl Groups for regex::CaptureLocations {
    fn len(&self) -> usize {
        regex::CaptureLocations::len(self)
    }
    fn span(&self, i: usize) -> Option<Span> {
        self.get(i)
    }
}

impl Groups for fancy_regex::Captures<'_, str> {
    fn len(&self) -> usize {
        fancy_regex::Captures::len(self)
    }
    fn span(&self, i: usize) -> Option<Span> {
        self.get(i).map(|m| (m.start(), m.end()))
    }
}

/// Groups that took part in the match, including group 0.
fn participating(groups: &dyn Groups) -> usize {
    (0..groups.len())
        .filter(|&i| groups.span(i).is_some())
        .count()
}

/// Every group's span, `None` for a group that did not take part.
fn group_spans(groups: &dyn Groups, offset: usize) -> Vec<Option<Span>> {
    (0..groups.len())
        .map(|i| {
            groups
                .span(i)
                .map(|(start, end)| (start + offset, end + offset))
        })
        .collect()
}

/// A model's work on one haystack, repeated by Rebar's timer, and replayed
/// call for call by `--verify`.
///
/// Engines with caller-owned storage keep it across records and iterations,
/// as a worker would: `regexrs-searcher` one `Searcher`, which every model
/// searches through, and one `CaptureLocations`, `regex` one
/// `CaptureLocations`. The other engines return an owned capture snapshot per
/// search. Each store is created by the first call that uses it, so a model
/// that never reads capture locations never allocates them, and a first run
/// pays for exactly the storage its calls use.
struct Workload<'a> {
    engine: &'a Engine,
    text: &'a str,
    searcher: Option<regexrs::Searcher<'a>>,
    locations: Option<regexrs::CaptureLocations>,
    regex_locations: Option<regex::CaptureLocations>,
}

impl<'a> Workload<'a> {
    fn new(engine: &'a Engine, text: &'a str) -> Self {
        Workload {
            engine,
            text,
            searcher: None,
            locations: None,
            regex_locations: None,
        }
    }

    /// Visit every non-overlapping match in `text`, in order.
    fn for_each_match(&mut self, text: &str, mut visit: impl FnMut(Span)) -> Result<()> {
        match self.engine {
            Engine::RegexrsSearcher(re) => {
                for m in self
                    .searcher
                    .get_or_insert_with(|| re.searcher())
                    .find_iter(text)
                {
                    let m = m?;
                    visit((m.start(), m.end()));
                }
            }
            Engine::Regexrs(re) | Engine::RegexrsNative(re) | Engine::RegexrsSeek(re) => {
                for m in re.find_iter(text) {
                    let m = m?;
                    visit((m.start(), m.end()));
                }
            }
            Engine::RegexrsRegular(re) => {
                re.find_iter(text).for_each(|m| visit((m.start(), m.end())));
            }
            Engine::Regex(re) => re.find_iter(text).for_each(|m| visit((m.start(), m.end()))),
            Engine::Fancy(re) | Engine::FancySeek(re) => {
                for m in re.find_iter(text) {
                    let m = m?;
                    visit((m.start(), m.end()));
                }
            }
            Engine::Resharp(re) => {
                for m in re.find_all(text.as_bytes()).map_err(|e| anyhow!("{e}"))? {
                    visit((m.start, m.end));
                }
            }
        }
        Ok(())
    }

    /// Visit the groups of every non-overlapping match in `text`, in order.
    fn for_each_captures(&mut self, text: &str, mut visit: impl FnMut(&dyn Groups)) -> Result<()> {
        match self.engine {
            Engine::RegexrsSearcher(re) => {
                let search = self.searcher.get_or_insert_with(|| re.searcher());
                for captures in search.captures_iter(text) {
                    visit(&captures?);
                }
            }
            Engine::Regexrs(re) | Engine::RegexrsNative(re) | Engine::RegexrsSeek(re) => {
                for captures in re.captures_iter(text) {
                    visit(&captures?);
                }
            }
            Engine::RegexrsRegular(re) => re.captures_iter(text).for_each(|c| visit(&c)),
            Engine::Regex(re) => re.captures_iter(text).for_each(|c| visit(&c)),
            Engine::Fancy(re) | Engine::FancySeek(re) => {
                for captures in re.captures_iter(text) {
                    visit(&captures?);
                }
            }
            Engine::Resharp(_) => bail!("resharp does not report captures here"),
        }
        Ok(())
    }

    /// Whether `line` matches, through the engine's boolean method (native
    /// code where the engine has it).
    fn is_match(&mut self, line: &str) -> Result<bool> {
        Ok(match self.engine {
            Engine::RegexrsSearcher(re) => self
                .searcher
                .get_or_insert_with(|| re.searcher())
                .is_match(line)?,
            Engine::Regexrs(re) | Engine::RegexrsNative(re) | Engine::RegexrsSeek(re) => {
                re.is_match(line)?
            }
            Engine::RegexrsRegular(re) => re.is_match(line),
            Engine::Regex(re) => re.is_match(line),
            Engine::Fancy(re) | Engine::FancySeek(re) => re.is_match(line)?,
            Engine::Resharp(re) => re.is_match(line.as_bytes()).map_err(|e| anyhow!("{e}"))?,
        })
    }

    /// One capture search of a record through the engine's per-record API,
    /// passing its first match's groups, or `None` without a match, to `read`.
    /// `regexrs-searcher` and `regex` read into their retained locations.
    fn first_record<T>(
        &mut self,
        line: &str,
        read: impl FnOnce(Option<&dyn Groups>) -> T,
    ) -> Result<T> {
        Ok(match self.engine {
            Engine::RegexrsSearcher(re) => {
                let search = self.searcher.get_or_insert_with(|| re.searcher());
                let locations = self.locations.get_or_insert_with(|| re.capture_locations());
                let found = search.captures_read(locations, line)?.is_some();
                read(found.then_some(&*locations as &dyn Groups))
            }
            Engine::Regexrs(re) | Engine::RegexrsNative(re) | Engine::RegexrsSeek(re) => {
                read(re.captures(line)?.as_ref().map(|c| c as &dyn Groups))
            }
            Engine::RegexrsRegular(re) => {
                read(re.captures(line).as_ref().map(|c| c as &dyn Groups))
            }
            Engine::Regex(re) => {
                let locations = self
                    .regex_locations
                    .get_or_insert_with(|| re.capture_locations());
                let found = re.captures_read(locations, line).is_some();
                read(found.then_some(&*locations as &dyn Groups))
            }
            Engine::Fancy(re) | Engine::FancySeek(re) => {
                read(re.captures(line)?.as_ref().map(|c| c as &dyn Groups))
            }
            Engine::Resharp(_) => bail!("resharp does not report captures here"),
        })
    }

    fn count_captures(&mut self, text: &str) -> Result<usize> {
        let mut total = 0;
        self.for_each_captures(text, |groups| total += participating(groups))?;
        Ok(total)
    }

    fn run(&mut self, model: &str) -> Result<usize> {
        let text = self.text;
        let mut total = 0;
        match model {
            "count" | "compile" => self.for_each_match(text, |_| total += 1)?,
            "replace" | "replace-capture" => total = replacement(self.engine, text, model)?.len(),
            "count-spans" => self.for_each_match(text, |(start, end)| total += end - start)?,
            "count-captures" => total = self.count_captures(text)?,
            "grep" => {
                for line in lines(text) {
                    total += usize::from(self.is_match(line)?);
                }
            }
            "grep-captures" => {
                for line in lines(text) {
                    total += self.count_captures(line)?;
                }
            }
            "first-captures-per-line" => {
                for line in lines(text) {
                    total += self.first_record(line, |groups| groups.map_or(0, participating))?;
                }
            }
            _ => bail!("unsupported benchmark model {model:?}"),
        }
        Ok(total)
    }
}

/// Lines with LF or CRLF terminators removed, as Rebar's line models require.
fn lines(text: &str) -> impl Iterator<Item = &str> {
    text.split_terminator('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
}

/// The model a `fresh-` or `startup-` model runs once per instance, and
/// whether construction is inside the timed interval.
fn first_run(model: &str) -> Option<(&str, bool)> {
    model
        .strip_prefix("fresh-")
        .map(|base| (base, false))
        .or_else(|| model.strip_prefix("startup-").map(|base| (base, true)))
}

fn measure(engine: &str, benchmark: &klv::Benchmark) -> Result<Vec<timer::Sample>> {
    let text = benchmark
        .haystack_str()
        .context("this runner requires UTF-8 haystacks")?;
    if let Some((base, constructed)) = first_run(&benchmark.model) {
        let first = |compiled: Engine| -> Result<(usize, Engine)> {
            let count = Workload::new(&compiled, text).run(base)?;
            Ok((count, compiled))
        };
        if constructed {
            return timer::run_and_count(
                benchmark,
                |(count, _compiled)| Ok(count),
                || first(Engine::compile(engine, benchmark)?),
            );
        }
        // The untimed step after each sample builds the next sample's instance.
        let next = std::cell::RefCell::new(Some(Engine::compile(engine, benchmark)?));
        return timer::run_and_count(
            benchmark,
            |(count, _compiled)| {
                next.replace(Some(Engine::compile(engine, benchmark)?));
                Ok(count)
            },
            || first(next.take().context("an instance per sample")?),
        );
    }
    if benchmark.model == "compile" {
        return timer::run_and_count(
            benchmark,
            |compiled: Engine| Workload::new(&compiled, text).run("compile"),
            || Engine::compile(engine, benchmark),
        );
    }
    let compiled = Engine::compile(engine, benchmark)?;
    let mut workload = Workload::new(&compiled, text);
    timer::run(benchmark, || workload.run(&benchmark.model))
}

fn replacement<'a>(
    engine: &Engine,
    text: &'a str,
    model: &str,
) -> Result<std::borrow::Cow<'a, str>> {
    let template = if model == "replace-capture" {
        "[${1}]"
    } else {
        "<$0>"
    };
    Ok(match engine {
        Engine::Regexrs(re) | Engine::RegexrsSeek(re) | Engine::RegexrsNative(re) => {
            re.try_replace_all(text, template)?
        }
        Engine::RegexrsRegular(re) => re.replace_all(text, template),
        Engine::Regex(re) => re.replace_all(text, template),
        Engine::Fancy(re) | Engine::FancySeek(re) => re.try_replacen(text, 0, template)?,
        _ => bail!("replacement model requires an engine with a replacement API"),
    })
}

fn is_line_model(model: &str) -> bool {
    matches!(model, "grep" | "grep-captures" | "first-captures-per-line")
}

/// What the calls `model` times report in one run over `text`, through a new
/// workload: for a fresh instance, exactly what its first run computes.
fn timed_report(engine: &Engine, text: &str, model: &str) -> Result<Report> {
    let mut workload = Workload::new(engine, text);
    let mut report = Report::default();
    for (offset, segment) in segments(text, model) {
        match model {
            "count" | "count-spans" | "compile" => workload.for_each_match(segment, |(s, e)| {
                report.matches.push((s + offset, e + offset))
            })?,
            "count-captures" | "grep-captures" => {
                let captures = report.captures.get_or_insert_with(Vec::new);
                workload.for_each_captures(segment, |groups| {
                    captures.push(group_spans(groups, offset))
                })?
            }
            "grep" => report.hits.push(workload.is_match(segment)?),
            "first-captures-per-line" => report.first.push(
                workload.first_record(segment, |groups| groups.map(|g| group_spans(g, offset)))?,
            ),
            _ => bail!("unsupported benchmark model {model:?}"),
        }
    }
    Ok(report)
}

/// The text a model searches with its offset: each line for line models,
/// otherwise the whole haystack.
fn segments<'t>(text: &'t str, model: &str) -> Vec<(usize, &'t str)> {
    if !is_line_model(model) {
        return vec![(0, text)];
    }
    let mut offset = 0;
    text.split_inclusive('\n')
        .map(|chunk| {
            let line = chunk.strip_suffix('\n').unwrap_or(chunk);
            let line = line.strip_suffix('\r').unwrap_or(line);
            let segment = (offset, line);
            offset += chunk.len();
            segment
        })
        .collect()
}

/// What one engine reports for a benchmark, with absolute offsets, gathered
/// through one `Workload` and the calls its timed model makes. Records keep
/// their file order, so retained storage sees alternating hits and misses.
#[derive(Debug, Default, PartialEq)]
struct Report {
    matches: Vec<Span>,
    /// `None` for an engine without captures.
    captures: Option<CaptureSpans>,
    /// `grep`: whether each line matches, from the boolean method.
    hits: Vec<bool>,
    /// `first-captures-per-line`: each line's first-match groups, from the
    /// per-record capture method.
    first: Vec<Option<Vec<Option<Span>>>>,
}

fn report(engine: &Engine, text: &str, model: &str) -> Result<Report> {
    let mut workload = Workload::new(engine, text);
    let mut report = Report {
        captures: engine.has_captures().then(Vec::new),
        ..Report::default()
    };
    for (offset, segment) in segments(text, model) {
        workload.for_each_match(segment, |(start, end)| {
            report.matches.push((start + offset, end + offset))
        })?;
        if let Some(captures) = report.captures.as_mut() {
            workload
                .for_each_captures(segment, |groups| captures.push(group_spans(groups, offset)))?;
        }
        match model {
            "grep" => report.hits.push(workload.is_match(segment)?),
            "first-captures-per-line" => report.first.push(
                workload.first_record(segment, |groups| groups.map(|g| group_spans(g, offset)))?,
            ),
            _ => {}
        }
    }
    Ok(report)
}

/// Fail at the first record where `actual` differs from the oracle.
fn check_same<T: PartialEq + std::fmt::Debug>(
    what: &str,
    actual: &[T],
    expected: &[T],
) -> Result<()> {
    match (0..actual.len().max(expected.len())).find(|&i| actual.get(i) != expected.get(i)) {
        Some(i) => bail!(
            "{what} {i} is {:?}, Fancy Regex reports {:?} ({} vs {} records)",
            actual.get(i),
            expected.get(i),
            actual.len(),
            expected.len()
        ),
        None => Ok(()),
    }
}

/// Compare everything the engine reports through its timed calls with Fancy
/// Regex: ordered match spans, every capture span, and for line models each
/// record's boolean result or first-match groups.
fn verify(engine: &str, benchmark: &klv::Benchmark) -> Result<()> {
    let text = benchmark
        .haystack_str()
        .context("this runner requires UTF-8 haystacks")?;
    let context = format!("{} {engine}:", benchmark.name);
    let (model, first) = match first_run(&benchmark.model) {
        Some((base, _)) => (base, true),
        None => (benchmark.model.as_str(), false),
    };
    if first {
        // The timed calls themselves, on instances nothing searched before.
        let found = timed_report(&Engine::compile(engine, benchmark)?, text, model)?;
        let expected = timed_report(&Engine::compile("fancy", benchmark)?, text, model)?;
        if found != expected {
            bail!("{context} first run reports {found:?}, Fancy Regex reports {expected:?}");
        }
    }
    let candidate = Engine::compile(engine, benchmark)?;
    if matches!(model, "replace" | "replace-capture") {
        let actual = replacement(&candidate, text, model)?;
        let oracle = replacement(&Engine::compile("fancy", benchmark)?, text, model)?;
        if actual != oracle {
            bail!("{context} replacement bytes differ");
        }
    }
    let found = report(&candidate, text, model)?;
    let expected = report(&Engine::compile("fancy", benchmark)?, text, model)?;
    check_same(
        &format!("{context} match"),
        &found.matches,
        &expected.matches,
    )?;
    if let (Some(captures), Some(oracle)) = (&found.captures, &expected.captures) {
        check_same(&format!("{context} capture set"), captures, oracle)?;
    }
    check_same(&format!("{context} line"), &found.hits, &expected.hits)?;
    check_same(
        &format!("{context} first captures of line"),
        &found.first,
        &expected.first,
    )?;
    let count = match first {
        true => Workload::new(&Engine::compile(engine, benchmark)?, text).run(model)?,
        false => Workload::new(&candidate, text).run(model)?,
    };
    println!(
        "verified {} {engine}: {} matches, {} capture sets, {} records, model count {count}",
        benchmark.name,
        found.matches.len(),
        found
            .captures
            .as_ref()
            .map_or_else(|| "no".to_string(), |c| c.len().to_string()),
        found.hits.len().max(found.first.len()),
    );
    Ok(())
}

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let engine = args
        .next()
        .with_context(|| format!("expected an engine: {ENGINES}"))?;
    let flag = args.next();
    if args.next().is_some() {
        bail!("unexpected trailing argument");
    }
    match flag.as_deref() {
        Some("--version") => {
            println!("{}", version(&engine)?);
            return Ok(());
        }
        Some("--verify") => {
            let benchmark = klv::Benchmark::read(std::io::stdin())?;
            return verify(&engine, &benchmark);
        }
        Some(other) => bail!("unexpected argument {other:?}"),
        None => {}
    }
    version(&engine)?;
    let benchmark = klv::Benchmark::read(std::io::stdin())?;
    let samples = measure(&engine, &benchmark)?;
    let mut stdout = std::io::stdout().lock();
    for sample in samples {
        writeln!(stdout, "{},{}", sample.duration.as_nanos(), sample.count)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn benchmark(pattern: &str, model: &str, haystack: &str) -> klv::Benchmark {
        klv::Benchmark {
            name: "test".into(),
            model: model.into(),
            regex: klv::Regex {
                patterns: vec![pattern.into()],
                unicode: true,
                case_insensitive: false,
            },
            haystack: std::sync::Arc::from(haystack.as_bytes()),
            max_iters: 1,
            max_warmup_iters: 0,
            max_time: std::time::Duration::from_secs(1),
            max_warmup_time: std::time::Duration::ZERO,
        }
    }

    const ENGINE_NAMES: [&str; 7] = [
        "regexrs",
        "regexrs-searcher",
        "regexrs-seek",
        "regexrs-regular",
        "regex",
        "fancy",
        "fancy-seek",
    ];

    #[test]
    fn every_engine_agrees_on_every_model() -> Result<()> {
        let text = "a1 b22\r\nc\n\n(x) 333\n";
        for (pattern, model, expected) in [
            (r"(\w)(\d+)?", "count", 5),
            (r"(\w)(\d+)?", "count-spans", 10),
            (r"(\w)(\d+)?", "count-captures", 13),
            (r"(\w)(\d+)?", "grep", 3),
            (r"(\w)(\d+)?", "grep-captures", 13),
            (r"(\w)(\d+)?", "first-captures-per-line", 7),
            ("", "count", 20),
        ] {
            for engine in ENGINE_NAMES {
                let b = benchmark(pattern, model, text);
                let compiled = Engine::compile(engine, &b)?;
                let count = Workload::new(&compiled, text).run(model)?;
                assert_eq!(count, expected, "{engine} {pattern:?} {model}");
            }
        }
        Ok(())
    }

    #[test]
    fn unmatched_and_empty_groups_are_distinct() -> Result<()> {
        let b = benchmark(r"(a)?(b*)", "count-captures", "bb");
        for engine in ENGINE_NAMES {
            verify(engine, &b)?;
        }
        Ok(())
    }

    #[test]
    fn matching_errors_are_failures_not_misses() {
        let b = benchmark(r"(a+)+\1$", "count", "aaaaaaaaaaaaaaaaaaaaaaaaaaaa!");
        let regex = regexrs::RegexBuilder::new(r"(a+)+\1$")
            .backtrack_limit(10)
            .build()
            .unwrap();
        let engine = Engine::Regexrs(regex);
        let mut workload = Workload::new(&engine, "aaaaaaaaaaaaaaaaaaaaaaaaaaaa!");
        assert!(workload.run(&b.model).is_err());
        assert!(workload.run("first-captures-per-line").is_err());
    }

    /// The per-record capture path, including `captures_read` into retained
    /// locations, reports each record's exact groups across alternating hits
    /// and misses, keeping unmatched groups distinct from empty ones.
    #[test]
    fn retained_storage_reports_exact_first_captures_per_record() -> Result<()> {
        type Record = Option<Vec<Option<Span>>>;
        let cases: [(&str, &[&str], Vec<Record>); 2] = [
            (
                r"(a)(b)?",
                &["ab", "x", "a", "ba", ""],
                vec![
                    Some(vec![Some((0, 2)), Some((0, 1)), Some((1, 2))]),
                    None,
                    Some(vec![Some((0, 1)), Some((0, 1)), None]),
                    Some(vec![Some((1, 2)), Some((1, 2)), None]),
                    None,
                ],
            ),
            (
                r"(a)?(b*)",
                &["bb", "zz", "ab", ""],
                vec![
                    Some(vec![Some((0, 2)), None, Some((0, 2))]),
                    Some(vec![Some((0, 0)), None, Some((0, 0))]),
                    Some(vec![Some((0, 2)), Some((0, 1)), Some((1, 2))]),
                    Some(vec![Some((0, 0)), None, Some((0, 0))]),
                ],
            ),
        ];
        for (pattern, records, expected) in cases {
            for engine in ENGINE_NAMES {
                let b = benchmark(pattern, "first-captures-per-line", "");
                let compiled = Engine::compile(engine, &b)?;
                let mut workload = Workload::new(&compiled, "");
                let mut found = Vec::new();
                for record in records {
                    found.push(workload.first_record(record, |g| g.map(|g| group_spans(g, 0)))?);
                }
                assert_eq!(found, expected, "{engine} {pattern}");
            }
        }
        Ok(())
    }

    /// Misreported spans or swapped hit records fail the gate even when the
    /// model's count is unchanged.
    #[test]
    fn verification_rejects_misreported_records() {
        let expected = [Some(vec![Some((0, 1))]), None];
        assert!(check_same("first", &expected, &expected).is_ok());
        assert!(check_same("first", &[Some(vec![Some((1, 2))]), None], &expected).is_err());
        assert!(check_same("first", &[None, Some(vec![Some((0, 1))])], &expected).is_err());
        assert!(check_same("first", &[Some(vec![None]), None], &expected).is_err());
        assert!(check_same("line", &[false, true], &[true, false]).is_err());
        assert!(check_same("line", &[true], &[true, false]).is_err());
    }

    /// Whole reports of the timed calls agree with the oracle for every
    /// engine and line model over alternating records.
    #[test]
    fn line_model_reports_match_the_oracle() -> Result<()> {
        let text = "ab
x

a
ba bab
";
        for model in ["grep", "grep-captures", "first-captures-per-line"] {
            let b = benchmark(r"(a)(b)?", model, text);
            let oracle = report(&Engine::compile("fancy", &b)?, text, model)?;
            assert_eq!(
                oracle.hits.len() + oracle.first.len(),
                if model == "grep-captures" { 0 } else { 5 }
            );
            for engine in ENGINE_NAMES {
                verify(engine, &b)?;
                assert_eq!(
                    report(&Engine::compile(engine, &b)?, text, model)?,
                    oracle,
                    "{engine} {model}"
                );
            }
        }
        Ok(())
    }

    /// The seek row runs the default row's methods on a regex whose program
    /// starts with the seek prefilter; the default row's never does.
    #[test]
    fn regexrs_seek_enables_only_the_seek_prefilter() -> Result<()> {
        let b = benchmark(r"\b(\w+)\s+\1\b", "count", "the the cat");
        let program = |engine| -> Result<String> {
            Ok(match Engine::compile(engine, &b)? {
                Engine::Regexrs(re) | Engine::RegexrsSeek(re) => {
                    regexrs::DebugRegex(&re).to_string()
                }
                _ => unreachable!(),
            })
        };
        assert!(program("regexrs-seek")?.contains("Seek"));
        assert!(!program("regexrs")?.contains("Seek"));
        verify("regexrs-seek", &b)
    }

    /// Retained storage is created by the first call that uses it: models
    /// that never read capture locations leave them unallocated, per-record
    /// captures allocate them once, and the searcher engine's searcher serves
    /// every model. First runs therefore pay for no storage their calls skip.
    #[test]
    fn retained_storage_is_created_only_by_its_first_use() -> Result<()> {
        let text = "ab\nx\n";
        let models = [
            "count",
            "count-spans",
            "count-captures",
            "grep",
            "grep-captures",
            "first-captures-per-line",
        ];
        for model in models {
            for engine in ENGINE_NAMES {
                let b = benchmark(r"(a)(b)?", model, text);
                let compiled = Engine::compile(engine, &b)?;
                let mut workload = Workload::new(&compiled, text);
                let unallocated = workload.searcher.is_none()
                    && workload.locations.is_none()
                    && workload.regex_locations.is_none();
                assert!(unallocated, "{engine} {model}");
                workload.run(model)?;
                let per_record = model == "first-captures-per-line";
                let context = format!("{engine} {model}");
                assert_eq!(
                    workload.searcher.is_some(),
                    engine == "regexrs-searcher",
                    "{context}"
                );
                assert_eq!(
                    workload.locations.is_some(),
                    per_record && engine == "regexrs-searcher",
                    "{context}"
                );
                assert_eq!(
                    workload.regex_locations.is_some(),
                    per_record && engine == "regex",
                    "{context}"
                );
            }
        }
        Ok(())
    }

    /// `fresh-` and `startup-` models run the base model once per new
    /// instance, verify its first run against the oracle, and report the base
    /// model's count for every sample.
    #[test]
    fn first_run_models_count_and_verify_like_their_base() -> Result<()> {
        let text = "ab\nx\n\na\nba bab\n";
        for base in [
            "count",
            "count-spans",
            "count-captures",
            "grep",
            "grep-captures",
            "first-captures-per-line",
        ] {
            let expected = {
                let b = benchmark(r"(a)(b)?", base, text);
                Workload::new(&Engine::compile("fancy", &b)?, text).run(base)?
            };
            for prefix in ["fresh-", "startup-"] {
                let mut b = benchmark(r"(a)(b)?", &format!("{prefix}{base}"), text);
                b.max_iters = 3;
                b.max_warmup_iters = 1;
                for engine in ENGINE_NAMES {
                    verify(engine, &b)?;
                    let samples = measure(engine, &b)?;
                    assert_eq!(samples.len(), 3, "{engine} {prefix}{base}");
                    assert!(samples.iter().all(|s| s.count == expected as u64));
                }
            }
        }
        assert_eq!(
            first_run("fresh-first-captures-per-line"),
            Some(("first-captures-per-line", false))
        );
        assert_eq!(first_run("startup-grep"), Some(("grep", true)));
        assert_eq!(first_run("first-captures-per-line"), None);
        Ok(())
    }

    #[test]
    fn line_models_strip_terminators() {
        assert_eq!(
            lines("foo\r\nbar\nfoo\r").collect::<Vec<_>>(),
            ["foo", "bar", "foo"]
        );
        assert_eq!(lines("").count(), 0);
    }

    #[test]
    fn leftmost_longest_differences_fail_verification() {
        let b = benchmark("a|ab", "count", "ab");
        assert!(verify("resharp", &b).is_err());
        let b = benchmark("ab|a", "count", "ab");
        assert!(verify("resharp", &b).is_ok());
    }
}
