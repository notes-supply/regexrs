//! Rebar BYOB runner for regexrs and current Rust regex engines.
//!
//! `regexrs-rebar-runner ENGINE [--version | --verify]` reads one benchmark
//! as KLV on stdin. Without a flag it times the benchmark's model with Rebar's
//! timer and prints `nanoseconds,count` samples. `--verify` checks the
//! engine's ordered match spans and every capture span, outside any timing,
//! against Fancy Regex as the leftmost-first oracle.

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

const ENGINES: &str = "regexrs, regexrs-native, regexrs-searcher, regexrs-regular, regex, \
                       fancy, fancy-seek, resharp";

fn version(engine: &str) -> Result<&'static str> {
    Ok(match engine {
        // The checked-out source; `mise run bench:rebar:measure` records its commit.
        "regexrs" | "regexrs-searcher" | "regexrs-regular" | "regexrs-native" => "workspace",
        "regex" => "1.13.1",
        "fancy" | "fancy-seek" => "0.19.2",
        "resharp" => "0.7.5",
        _ => bail!("unknown engine {engine:?}; expected one of {ENGINES}"),
    })
}

fn regexrs_options(benchmark: &klv::Benchmark, native: bool) -> regexrs::RegexOptionsBuilder {
    let mut options = regexrs::RegexOptionsBuilder::new();
    options
        .unicode_mode(benchmark.regex.unicode)
        .case_insensitive(benchmark.regex.case_insensitive)
        .native(native);
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
            "regexrs" => Engine::Regexrs(regexrs_options(benchmark, false).build(pattern.into())?),
            "regexrs-searcher" => {
                Engine::RegexrsSearcher(regexrs_options(benchmark, false).build(pattern.into())?)
            }
            "regexrs-regular" => Engine::RegexrsRegular(
                regexrs_options(benchmark, false).build_regular(pattern.into())?,
            ),
            "regexrs-native" if cfg!(feature = "native") => {
                Engine::RegexrsNative(regexrs_options(benchmark, true).build(pattern.into())?)
            }
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

    /// Ordered spans of all non-overlapping matches.
    fn match_spans(&self, text: &str) -> Result<Vec<Span>> {
        Ok(match self {
            Engine::Regexrs(re) | Engine::RegexrsNative(re) => re
                .find_iter(text)
                .map(|m| m.map(|m| (m.start(), m.end())))
                .collect::<regexrs::Result<_>>()?,
            Engine::RegexrsSearcher(re) => re
                .searcher()
                .find_iter(text)
                .map(|m| m.map(|m| (m.start(), m.end())))
                .collect::<regexrs::Result<_>>()?,
            Engine::RegexrsRegular(re) => {
                re.find_iter(text).map(|m| (m.start(), m.end())).collect()
            }
            Engine::Regex(re) => re.find_iter(text).map(|m| (m.start(), m.end())).collect(),
            Engine::Fancy(re) | Engine::FancySeek(re) => re
                .find_iter(text)
                .map(|m| m.map(|m| (m.start(), m.end())))
                .collect::<fancy_regex::Result<_>>()?,
            Engine::Resharp(re) => re
                .find_all(text.as_bytes())
                .map_err(|e| anyhow!("{e}"))?
                .into_iter()
                .map(|m| (m.start, m.end))
                .collect(),
        })
    }

    /// Every capture span of all non-overlapping matches, or `None` for an
    /// engine without captures.
    fn capture_spans(&self, text: &str) -> Result<Option<CaptureSpans>> {
        fn spans<'t>(
            groups: impl Iterator<Item = Option<(usize, usize)>> + 't,
        ) -> Vec<Option<Span>> {
            groups.collect()
        }
        Ok(Some(match self {
            Engine::Regexrs(re) | Engine::RegexrsNative(re) => re
                .captures_iter(text)
                .map(|c| c.map(|c| spans(c.iter().map(|m| m.map(|m| (m.start(), m.end()))))))
                .collect::<regexrs::Result<_>>()?,
            Engine::RegexrsSearcher(re) => re
                .searcher()
                .captures_iter(text)
                .map(|c| c.map(|c| spans(c.iter().map(|m| m.map(|m| (m.start(), m.end()))))))
                .collect::<regexrs::Result<_>>()?,
            Engine::RegexrsRegular(re) => re
                .captures_iter(text)
                .map(|c| spans(c.iter().map(|m| m.map(|m| (m.start(), m.end())))))
                .collect(),
            Engine::Regex(re) => re
                .captures_iter(text)
                .map(|c| spans(c.iter().map(|m| m.map(|m| (m.start(), m.end())))))
                .collect(),
            Engine::Fancy(re) | Engine::FancySeek(re) => re
                .captures_iter(text)
                .map(|c| c.map(|c| spans(c.iter().map(|m| m.map(|m| (m.start(), m.end()))))))
                .collect::<fancy_regex::Result<_>>()?,
            Engine::Resharp(_) => return Ok(None),
        }))
    }
}

/// A model's work on one haystack, repeated by Rebar's timer. Searcher-based
/// engines keep one searcher (and capture storage) across iterations, as a
/// worker would.
struct Workload<'a> {
    engine: &'a Engine,
    text: &'a str,
    searcher: Option<regexrs::Searcher<'a>>,
    locations: Option<regexrs::CaptureLocations>,
}

fn participating(groups: impl Iterator<Item = bool>) -> usize {
    groups.filter(|&participated| participated).count()
}

impl<'a> Workload<'a> {
    fn new(engine: &'a Engine, text: &'a str) -> Self {
        let (searcher, locations) = match engine {
            Engine::RegexrsSearcher(re) => (Some(re.searcher()), Some(re.capture_locations())),
            _ => (None, None),
        };
        Workload {
            engine,
            text,
            searcher,
            locations,
        }
    }

    fn count(&mut self, text: &str, spans: bool) -> Result<usize> {
        let size = |(start, end): Span| if spans { end - start } else { 1 };
        let mut total = 0;
        match (self.engine, self.searcher.as_mut()) {
            (Engine::RegexrsSearcher(_), Some(search)) => {
                for m in search.find_iter(text) {
                    let m = m?;
                    total += size((m.start(), m.end()));
                }
            }
            (Engine::Regexrs(re) | Engine::RegexrsNative(re) | Engine::RegexrsSearcher(re), _) => {
                for m in re.find_iter(text) {
                    let m = m?;
                    total += size((m.start(), m.end()));
                }
            }
            (Engine::RegexrsRegular(re), _) => {
                total = re.find_iter(text).map(|m| size((m.start(), m.end()))).sum();
            }
            (Engine::Regex(re), _) => {
                total = re.find_iter(text).map(|m| size((m.start(), m.end()))).sum();
            }
            (Engine::Fancy(re) | Engine::FancySeek(re), _) => {
                for m in re.find_iter(text) {
                    let m = m?;
                    total += size((m.start(), m.end()));
                }
            }
            (Engine::Resharp(re), _) => {
                let matches = re.find_all(text.as_bytes()).map_err(|e| anyhow!("{e}"))?;
                total = matches.into_iter().map(|m| size((m.start, m.end))).sum();
            }
        }
        Ok(total)
    }

    /// Participating groups, including group 0, over all matches in `text`.
    fn count_captures(&mut self, text: &str) -> Result<usize> {
        let mut total = 0;
        match (self.engine, self.searcher.as_mut()) {
            (Engine::RegexrsSearcher(_), Some(search)) => {
                for captures in search.captures_iter(text) {
                    total += participating(captures?.iter().map(|m| m.is_some()));
                }
            }
            (Engine::Regexrs(re) | Engine::RegexrsNative(re) | Engine::RegexrsSearcher(re), _) => {
                for captures in re.captures_iter(text) {
                    total += participating(captures?.iter().map(|m| m.is_some()));
                }
            }
            (Engine::RegexrsRegular(re), _) => {
                for captures in re.captures_iter(text) {
                    total += participating(captures.iter().map(|m| m.is_some()));
                }
            }
            (Engine::Regex(re), _) => {
                for captures in re.captures_iter(text) {
                    total += participating(captures.iter().map(|m| m.is_some()));
                }
            }
            (Engine::Fancy(re) | Engine::FancySeek(re), _) => {
                for captures in re.captures_iter(text) {
                    total += participating(captures?.iter().map(|m| m.is_some()));
                }
            }
            (Engine::Resharp(_), _) => bail!("resharp does not report captures here"),
        }
        Ok(total)
    }

    fn is_match(&mut self, line: &str) -> Result<bool> {
        Ok(match (self.engine, self.searcher.as_mut()) {
            (Engine::RegexrsSearcher(_), Some(search)) => search.is_match(line)?,
            (Engine::Regexrs(re) | Engine::RegexrsNative(re) | Engine::RegexrsSearcher(re), _) => {
                re.is_match(line)?
            }
            (Engine::RegexrsRegular(re), _) => re.is_match(line),
            (Engine::Regex(re), _) => re.is_match(line),
            (Engine::Fancy(re) | Engine::FancySeek(re), _) => re.is_match(line)?,
            (Engine::Resharp(re), _) => re.is_match(line.as_bytes()).map_err(|e| anyhow!("{e}"))?,
        })
    }

    /// Participating groups of the first match in `line`: one capture search
    /// per record, into caller-owned storage where the engine offers it.
    fn first_captures(&mut self, line: &str) -> Result<usize> {
        let groups = |found: Option<regexrs::Captures<'_>>| {
            found.map_or(0, |c| participating(c.iter().map(|m| m.is_some())))
        };
        Ok(
            match (self.engine, self.searcher.as_mut(), self.locations.as_mut()) {
                (Engine::RegexrsSearcher(_), Some(search), Some(locations)) => {
                    if search.captures_read(locations, line)?.is_some() {
                        participating((0..locations.len()).map(|i| locations.get(i).is_some()))
                    } else {
                        0
                    }
                }
                (
                    Engine::Regexrs(re) | Engine::RegexrsNative(re) | Engine::RegexrsSearcher(re),
                    ..,
                ) => groups(re.captures(line)?),
                (Engine::RegexrsRegular(re), ..) => groups(re.captures(line)),
                (Engine::Regex(re), ..) => {
                    let mut locations = re.capture_locations();
                    if re.captures_read(&mut locations, line).is_some() {
                        participating((0..locations.len()).map(|i| locations.get(i).is_some()))
                    } else {
                        0
                    }
                }
                (Engine::Fancy(re) | Engine::FancySeek(re), ..) => re
                    .captures(line)?
                    .map_or(0, |c| participating(c.iter().map(|m| m.is_some()))),
                (Engine::Resharp(_), ..) => bail!("resharp does not report captures here"),
            },
        )
    }

    fn run(&mut self, model: &str) -> Result<usize> {
        let text = self.text;
        match model {
            "count" | "compile" => self.count(text, false),
            "count-spans" => self.count(text, true),
            "count-captures" => self.count_captures(text),
            "grep" => lines(text).try_fold(0, |n, line| Ok(n + usize::from(self.is_match(line)?))),
            "grep-captures" => {
                lines(text).try_fold(0, |n, line| Ok(n + self.count_captures(line)?))
            }
            "first-captures-per-line" => {
                lines(text).try_fold(0, |n, line| Ok(n + self.first_captures(line)?))
            }
            _ => bail!("unsupported benchmark model {model:?}"),
        }
    }
}

/// Lines with LF or CRLF terminators removed, as Rebar's line models require.
fn lines(text: &str) -> impl Iterator<Item = &str> {
    text.split_terminator('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
}

fn measure(engine: &str, benchmark: &klv::Benchmark) -> Result<Vec<timer::Sample>> {
    let text = benchmark
        .haystack_str()
        .context("this runner requires UTF-8 haystacks")?;
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

/// The text a model searches: each line, with its offset, for line models;
/// otherwise the whole haystack.
fn segments<'t>(text: &'t str, model: &str) -> Vec<(usize, &'t str)> {
    if !matches!(model, "grep" | "grep-captures" | "first-captures-per-line") {
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

/// Spans and captures over every segment, with absolute offsets.
fn all_spans(
    engine: &Engine,
    text: &str,
    model: &str,
) -> Result<(Vec<Span>, Option<CaptureSpans>)> {
    let shift = |offset: usize, (start, end): Span| (start + offset, end + offset);
    let mut matches = Vec::new();
    let mut captures = Some(Vec::new());
    for (offset, segment) in segments(text, model) {
        matches.extend(
            engine
                .match_spans(segment)?
                .into_iter()
                .map(|span| shift(offset, span)),
        );
        match (engine.capture_spans(segment)?, captures.as_mut()) {
            (Some(sets), Some(all)) => all.extend(sets.into_iter().map(|set| {
                set.into_iter()
                    .map(|span| span.map(|span| shift(offset, span)))
                    .collect()
            })),
            _ => captures = None,
        }
    }
    Ok((matches, captures))
}

/// Compare an engine's complete spans and captures with Fancy Regex's, over
/// the segments the benchmark's model searches.
fn verify(engine: &str, benchmark: &klv::Benchmark) -> Result<()> {
    let text = benchmark
        .haystack_str()
        .context("this runner requires UTF-8 haystacks")?;
    let candidate = Engine::compile(engine, benchmark)?;
    let oracle = Engine::compile("fancy", benchmark)?;
    let (matches, captures) = all_spans(&candidate, text, &benchmark.model)?;
    let (expected, expected_captures) = all_spans(&oracle, text, &benchmark.model)?;
    if let Some(i) =
        (0..matches.len().max(expected.len())).find(|&i| matches.get(i) != expected.get(i))
    {
        bail!(
            "{} {engine}: match {i} is {:?}, Fancy Regex reports {:?} ({} vs {} matches)",
            benchmark.name,
            matches.get(i),
            expected.get(i),
            matches.len(),
            expected.len()
        );
    }
    if let Some(captures) = &captures
        && Some(captures) != expected_captures.as_ref()
    {
        bail!(
            "{} {engine}: capture spans differ from Fancy Regex",
            benchmark.name
        );
    }
    let count = Workload::new(&candidate, text).run(&benchmark.model)?;
    println!(
        "verified {} {engine}: {} matches, {} capture sets, model count {count}",
        benchmark.name,
        matches.len(),
        captures.map_or_else(|| "no".to_string(), |c| c.len().to_string())
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

    const ENGINE_NAMES: [&str; 6] = [
        "regexrs",
        "regexrs-searcher",
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
