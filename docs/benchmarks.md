# Rebar benchmark suite

`benchmarks/` is a [Rebar](https://github.com/BurntSushi/rebar) "bring your own
benchmarks" suite comparing regexrs with current Rust regex engines. It is
development tooling only: excluded from the crate package and kept out of
regexrs's dependency graph by its own Cargo workspace.

## Running

```sh
mise run bench:rebar:verify   # fetch, build, then check every row
mise run bench:rebar:measure  # verify, then time every row
mise run bench:rebar:smoke    # short run that exercises the harness
```

`bench:rebar:fetch` clones Rebar at the pinned commit
`463d00f31887e84c38467805b9e3122c314b9521` into the ignored `benchmarks/.rebar`.
That one checkout provides the `rebar` CLI, the KLV reader and timer the runner
reuses, and the real-text corpora, linked into `benchmarks/rebar/haystacks/rebar`
and `benchmarks/rebar/regexes/rebar`. `measure` writes `results.csv` and a
`source.txt` (commit, worktree status, toolchain, host) under the ignored
`.benchmark-results/rebar/`. Use `rebar cmp`/`rebar rank` from
`benchmarks/target/release/rebar` on those files.

The toolchain is the project's Mise configuration. Runner dependencies are
pinned in `benchmarks/runner/Cargo.lock`: regex 1.13.1, fancy-regex 0.19.2 and
resharp 0.7.5. Earlier comparisons against fancy-regex 0.19.1 are historical.

### Revision comparisons

`mise run bench:rebar:compare -- LABEL=COMMIT LABEL=COMMIT ...` freezes committed
sources and compares them on one CPU with the same adapter, fixtures and lock.
Use full commit IDs. Results and verification logs go to
`.benchmark-results/comparison/`; use a fresh output directory for each run.

The default quick mode uses the 18 representative cases in
`benchmarks/quick-cases.txt`, with five rotated repetitions. It covers construction,
first use, Unicode, captures, replacement, misses and backreferences. Set
`BENCH_MODE=full` for every workload and the requested revisions. Quick mode is a
regression signal, not complete coverage or an overall engine ranking.

The benchmark workflow defaults to quick mode for benchmark-branch pushes; its
manual `mode` input selects quick or full. It caches tools and Rust dependencies,
not measured answers. The selected library is rebuilt to prevent backdated
archived sources from reusing stale product code. All verification and timed
operations still run. Hosted-VM results are relative comparisons; do not combine
their absolute times with measurements from another machine.

## Correctness gates

Nothing is timed until `verify.sh` passes for every row:

1. The runner's `--verify` mode replays the calls each timed model makes,
   through the same per-engine code and retained storage. It compares the
   engine's ordered match spans and every capture span against fancy-regex as
   the leftmost-first oracle, keeping unmatched groups distinct from empty
   ones. Line models are compared line by line, with absolute offsets, and in
   file order, so retained storage sees alternating hits and misses:
   - `grep` checks every line's result from the boolean method that is timed,
     native code included;
   - `first-captures-per-line` checks every line's exact first-match groups,
     or no match, from the per-record capture method that is timed.
2. `rebar measure -t` checks each row's expected `count`, which Rebar also
   checks on every timed iteration.

Matching errors, including execution-limit errors, fail the row; they are never
counted as misses.

## Models

The standard Rebar models, as defined in Rebar's `MODELS.md`: `compile`,
`count`, `count-spans`, `count-captures` (participating groups, including group
0), `grep` (matching lines) and `grep-captures`. Line models strip LF and CRLF
terminators.

`first-captures-per-line` is a supplement: one capture search per line,
counting the participating groups of its first match. It is the record
extraction case where caller-owned capture storage matters, and is only
compared within `definitions/scratch.toml`.

`replace` applies `<$0>` to every match; `replace-capture` applies `[${1}]`.
They count output bytes, with complete replacement bytes checked against Fancy
Regex before timing. Both include construction of the replacement result.

Two prefixes time a regex's first use with any of these models:

- `fresh-MODEL` times the first run of MODEL on an instance constructed outside
  the timed interval and never searched before. Lazily built engines, caches
  and code are inside the interval.
- `startup-MODEL` times construction plus that first run.

Every sample uses a new instance, and instances are dropped outside the
interval. `compile` times construction alone, and the plain models time warmed
matching. `--verify` also replays the timed calls of a first run on fresh
instances of the engine and the oracle.

Capture APIs differ by engine, and each row uses the engine's API as listed:

- **Reusable storage:** `regexrs-searcher` (`Searcher::captures_read` into one
  `CaptureLocations`) and `regex` (`Regex::captures_read` into one
  `CaptureLocations`). Both create their storage once, outside the timed loop,
  and reuse it across records and iterations.
- **Owned snapshots:** `regexrs`, `regexrs-seek`, `regexrs-regular`, `fancy`
  and `fancy-seek` return an owned `Captures` per record.

Compare reusable rows with reusable rows, and snapshot rows with snapshot rows.
A gap between the two kinds reflects the API choice, not engine speed.

## Engines and configurations

| engine | configuration |
|---|---|
| `regexrs` | default `Regex`, native code disabled (the default build) |
| `regexrs-native` | `Regex` with the `native` feature; boolean searches only |
| `regexrs-searcher` | `Regex` through one reused `Searcher` and `CaptureLocations` |
| `regexrs-seek` | `Regex` with its opt-in seek prefilter, `seek(true)`, and native code disabled; the same methods as `regexrs` |
| `regexrs-regular` | `RegularRegex`: no backtracking; a plan, or regex-automata with its bounded backtracker disabled |
| `regex` | regex 1.13.1, defaults |
| `fancy` | fancy-regex 0.19.2, defaults |
| `fancy-seek` | fancy-regex with its opt-in seek prefilter |
| `resharp` | resharp 0.7.5, full Unicode classes, `^`/`$` at text boundaries |

Defaults and tuned configurations are separate engines and must not be merged
into one result. `regexrs-seek` is the counterpart of `fancy-seek`: seek stays
opt-in because it can regress hit-heavy searches, and the default route is not
chosen to improve a table. resharp is leftmost-longest: it is listed only on rows whose
patterns avoid its syntax extensions (`_`, `&`, `~`) and where verification
shows the same spans as leftmost-first engines. Where it differs, the row fails
verification instead of being compared.

## Definitions

All rows use Unicode mode.

- `ordinary.toml`: regular patterns on real text, with `regexrs-seek` beside
  the defaults, including literal hits,
  case-insensitive hits, a selective miss, a name alternation, dense words,
  dialogue spans, Cyrillic words, line-oriented keyword search, captured
  function names and the real-world log extraction regex.
- `rich.toml`: backreferences and lookaround, with `regexrs-seek` and
  `fancy-seek` beside the defaults. These include doubled words
  (whole text and per line), a selective per-line miss, names after titles,
  words before `!`, Rust raw strings and method calls.
  A whole-text search for the miss `\b(\w+) \1 \1\b` exceeds the default
  backtrack limit in regexrs and fancy-regex alike, so that row is per line.
- `regular.toml`: `RegularRegex` against `Regex` and the regex crate, with
  regexrs-searcher for retained captures and per-line booleans. Families cover:
  - dense class runs (ASCII, Cyrillic and Han)
  - quoted fields, literal sets and alternation
  - Unicode word boundaries and case folding
  - finite repeats (log dates and UUIDs), anchors and multi-line lines
  - greedy and lazy runs, word-bounded runs and folded literals
  - captured log records

  Each family also has `compile`, `fresh-MODEL`, `startup-MODEL` and warmed rows
  on a one-line store, a hundred-line store and whole haystacks.
- `scratch.toml`: `Searcher` and `CaptureLocations` against plain calls.
- `edge.toml`: small checked-in fixtures for empty matches at UTF-8
  boundaries, unmatched versus empty groups, and case folds of different
  UTF-8 widths.
- `planning.toml`: construction, first use and warmed matching, kept apart, for
  title lookbehind hits and a full miss, method calls, a dense lookbehind,
  repeated and tripled words, a regular class run, literal hit and miss
  controls and the routed keyword lines. Each case has `compile`,
  `fresh-MODEL` and `startup-MODEL` rows on a store of about a hundred lines,
  and the warmed model on the whole haystack; some also run `fresh-MODEL` on
  the whole haystack. regexrs, regexrs-seek and fancy are on every case.
  fancy-seek is on the cases fancy runs on its backtracking VM, the lookaround,
  repeated-word and keyword cases, but not on the class-run and literal
  controls, where fancy delegates the whole pattern and seek does nothing.
  regexrs-regular, regex and resharp only appear where their semantics match.

The suite has 112 definitions and 464 engine rows. The 219 `planning/` rows
and the 128 `regular/` rows can be selected or excluded with Rebar's `-f`/`-F`.

### Comparing revisions

`benchmarks/revision.sh` builds this checkout's runner adapter, with its lock,
features and definitions, against any regexrs source tree, so two revisions run
the same calls on the same fixtures:

```sh
git archive 9ebb13d | (mkdir -p /tmp/base && tar -x -C /tmp/base)
mise run bench:rebar:revision build /tmp/base 9ebb13d
mise run bench:rebar:revision build "$PWD" candidate
mise run bench:rebar:revision verify 9ebb13d
mise run bench:rebar:revision verify candidate
mise run bench:rebar:revision verify candidate '^regular/'   # any row filter
cd benchmarks
taskset -c 2 target/release/rebar measure -d target/planning/9ebb13d/suite \
  -f '^planning/' > base.csv
taskset -c 2 target/release/rebar measure -d target/planning/candidate/suite \
  -f '^planning/' > candidate.csv
target/release/rebar diff base.csv candidate.csv
```

Each label gets `target/planning/LABEL/regexrs-rebar-runner` and a suite
directory whose engines run it. Revision builds share `target/planning/cargo`.
Alternate the revision order across repeated runs.

### Provenance

- From the pinned Rebar checkout, not redistributed here:
  - `sherlock.txt`, *The Adventures of Sherlock Holmes* from Project Gutenberg, public domain.
  - `opensubtitles/ru-sampled.txt` and `opensubtitles/zh-sampled.txt`, samples of the OPUS OpenSubtitles 2018 corpus.
  - `rust-src-tools-3b0d4813.txt`, Rust project source, MIT/Apache-2.0.
  - `wild/unstructured-to-json.log` and its regex, from a public regex discussion; see Rebar's `curated/11-unstructured-to-json.toml`.
- Checked in: `haystacks/synthetic/` holds three small hand-written fixtures.

Expected counts come from the oracle. The regex crate agrees on every regular
row, and resharp on its rows.

## Interpreting results

This is a curated, intentionally selective suite, as Rebar describes its own.
It is not a representative sample of regex use. Report each row on its own:
losses stay visible, and there is no single cross-workload score.

Public speed claims need repeated, rotated runs on a quiet physical host with a
pinned CPU, with the same toolchain and recorded sources. A difference smaller
than the run-to-run variation is inconclusive. Short runs, and anything on a
shared VM, only show that the harness works.

The suite does not measure consumer build time, binary size, retained memory,
allocations, concurrency or adversarial scaling; those need separate tools.

## Planning and construction pass, 2026-10-11

This pair compares released 0.1.4, `9ebb13d2d262a7c821ab22efad33e917ca75d67f`,
with `bd59bf9f99aa561e3fc67733949679d71ce9613d`. Both use the candidate's
adapter, dependency lock, fixtures, features and pinned compiler. The 0.1.5
release changes package metadata and documentation, not the measured library.
Earlier 0.1.4 results and README tables are preserved.

The host was an AMD Ryzen 9 9950X3D running Linux 7.2.5-3-omarchy, with the
performance governor and nightly-2026-10-11. Each process was pinned to CPU 2.
Its SMT sibling was not reserved. This is a shared workstation. No task builds
or profiling ran during timing.

Both revisions passed 11 runner tests and exact output and count checks for all
351 rows before timing. Six repetitions rotated and reversed engine order and
alternated revision order per pair. Nine engine positions are not completely
balanced by six repetitions. Each row used a 50 ms warmup and a 200 ms measurement
budget, up to 10 million iterations. All 4,212 timed rows passed count checks.

Results are medians of six run medians. Ranges are their minimum and maximum,
not confidence intervals. `compile` measures pattern construction without a
search. `fresh-MODEL` measures the first actual model run on a new instance,
constructed outside the interval. `startup-MODEL` includes both construction
and that first run. Plain models are warmed and retain their working storage.
No first-use result is inferred from a warmed search.

- [All paired measurements](../benchmarks/results/2026-10-11-planning/measurements.csv).
- [Every median and range, in nanoseconds](../benchmarks/results/2026-10-11-planning/summary.csv).
- [Source, host, compiler, budgets and exact orders](../benchmarks/results/2026-10-11-planning/metadata.json).

The implementation avoids unused seek-pattern construction and lets an opt-in
seek use leading-lookbehind candidates. It does not turn seek on by default.
Under explicit backtrack limits, seek still leads and rejected candidates retain
their failure cost. With the default limit, a lookbehind-led seek can return the
correct answer where the old path exhausted its budget on impossible positions.

Selected before/after results, including construction and small-store startup:

| Workload | Configuration | 0.1.4 | Candidate | Change | Ranges overlap |
|---|---|---:|---:|---:|---|
| `titled-compile` | `regexrs` | 136.26 µs | 136.12 µs | -0.1% | yes |
| `titled-compile` | `regexrs-seek` | 281.38 µs | 265.39 µs | -5.7% | yes |
| `titled-fresh-small` | `regexrs` | 5.63 µs | 5.87 µs | +4.3% | no |
| `titled-fresh-small` | `regexrs-seek` | 38.12 µs | 8.82 µs | -76.8% | no |
| `titled-startup-small` | `regexrs` | 141.81 µs | 141.95 µs | +0.1% | yes |
| `titled-startup-small` | `regexrs-seek` | 326.50 µs | 281.98 µs | -13.6% | yes |
| `titled-warm` | `regexrs` | 64.58 µs | 65.40 µs | +1.3% | yes |
| `titled-warm` | `regexrs-seek` | 1.30 ms | 54.91 µs | -95.8% | no |
| `titled-miss-compile` | `regexrs` | 135.72 µs | 134.84 µs | -0.7% | yes |
| `titled-miss-compile` | `regexrs-seek` | 274.13 µs | 290.36 µs | +5.9% | yes |
| `titled-miss-fresh-small` | `regexrs` | 321.0 ns | 315.5 ns | -1.7% | yes |
| `titled-miss-fresh-small` | `regexrs-seek` | 34.20 µs | 350.0 ns | -99.0% | no |
| `titled-miss-startup-small` | `regexrs` | 135.95 µs | 135.72 µs | -0.2% | yes |
| `titled-miss-startup-small` | `regexrs-seek` | 313.83 µs | 280.81 µs | -10.5% | yes |
| `titled-miss-warm` | `regexrs` | 25.82 µs | 25.82 µs | -0.0% | yes |
| `titled-miss-warm` | `regexrs-seek` | 1.30 ms | 25.80 µs | -98.0% | no |
| `method-calls-compile` | `regexrs` | 1.95 µs | 1.90 µs | -2.6% | yes |
| `method-calls-compile` | `regexrs-seek` | 6.28 µs | 6.20 µs | -1.3% | no |
| `method-calls-fresh-small` | `regexrs` | 1.65 µs | 1.67 µs | +1.2% | yes |
| `method-calls-fresh-small` | `regexrs-seek` | 9.56 µs | 2.67 µs | -72.1% | no |
| `method-calls-startup-small` | `regexrs` | 3.57 µs | 3.58 µs | +0.3% | yes |
| `method-calls-startup-small` | `regexrs-seek` | 15.95 µs | 8.83 µs | -44.6% | no |
| `method-calls-warm` | `regexrs` | 6.33 ms | 6.36 ms | +0.4% | yes |
| `method-calls-warm` | `regexrs-seek` | 16.87 ms | 3.44 ms | -79.6% | no |
| `after-space-compile` | `regexrs` | 125.19 µs | 125.07 µs | -0.1% | yes |
| `after-space-compile` | `regexrs-seek` | 274.79 µs | 281.11 µs | +2.3% | yes |
| `after-space-fresh-small` | `regexrs` | 73.83 µs | 73.55 µs | -0.4% | yes |
| `after-space-fresh-small` | `regexrs-seek` | 117.72 µs | 119.06 µs | +1.1% | yes |
| `after-space-startup-small` | `regexrs` | 199.40 µs | 198.00 µs | -0.7% | yes |
| `after-space-startup-small` | `regexrs-seek` | 387.30 µs | 392.83 µs | +1.4% | yes |
| `after-space-warm` | `regexrs` | 9.12 ms | 9.04 ms | -0.9% | yes |
| `after-space-warm` | `regexrs-seek` | 9.54 ms | 9.61 ms | +0.7% | yes |
| `rust-keywords-compile` | `regexrs` | 3.74 µs | 3.54 µs | -5.3% | no |
| `rust-keywords-compile` | `regexrs-seek` | 37.88 µs | 37.34 µs | -1.4% | no |
| `rust-keywords-fresh-small` | `regexrs` | 27.07 µs | 26.48 µs | -2.2% | no |
| `rust-keywords-fresh-small` | `regexrs-seek` | 27.26 µs | 26.89 µs | -1.3% | no |
| `rust-keywords-startup-small` | `regexrs` | 30.16 µs | 29.56 µs | -2.0% | yes |
| `rust-keywords-startup-small` | `regexrs-seek` | 65.28 µs | 64.20 µs | -1.6% | no |
| `rust-keywords-warm` | `regexrs` | 11.66 ms | 11.68 ms | +0.1% | yes |
| `rust-keywords-warm` | `regexrs-seek` | 11.72 ms | 11.70 ms | -0.2% | yes |

Positive changes are slower. Overlapping ranges do not establish a difference.

Non-overlapping regressions above 2%, including unchanged configurations:

- `planning/tripled-words-compile` with `regexrs-seek`: 577.30 µs to 610.70 µs, 5.8% slower.
- `planning/tripled-words-compile` with `fancy-seek`: 577.20 µs to 605.90 µs, 5.0% slower.
- `scratch/doubled-words-records` with `regexrs-searcher`: 20.34 ms to 21.22 ms, 4.4% slower.
- `planning/tripled-words-startup-small` with `regexrs-seek`: 774.27 µs to 807.67 µs, 4.3% slower.
- `planning/titled-fresh-small` with `regexrs`: 5.63 µs to 5.87 µs, 4.3% slower.
- `edge/empty-utf8-boundaries` with `fancy`: 365.5 ns to 381.0 ns, 4.2% slower.
- `planning/literal-miss-compile` with `regexrs`: 1.07 µs to 1.12 µs, 4.2% slower.
- `planning/tripled-words-startup-small` with `fancy-seek`: 780.99 µs to 813.32 µs, 4.1% slower.
- `planning/tripled-words-compile` with `fancy`: 2.08 µs to 2.15 µs, 3.9% slower.
- `scratch/doubled-words-lines` with `regexrs-searcher`: 20.50 ms to 21.20 ms, 3.4% slower.
- `planning/doubled-words-compile` with `fancy`: 2.17 µs to 2.24 µs, 3.0% slower.
- `planning/doubled-words-compile` with `regexrs-seek`: 277.83 µs to 285.89 µs, 2.9% slower.
- `rich/compile-doubled-words` with `regexrs-seek`: 298.64 µs to 306.45 µs, 2.6% slower.
- `planning/doubled-words-compile` with `fancy-seek`: 278.12 µs to 284.76 µs, 2.4% slower.
- `rich/exclamations` with `regexrs-seek`: 25.84 µs to 26.36 µs, 2.0% slower.

Tripled-word seek construction was 5.8% slower, while the unchanged fancy-seek
configuration was 5.0% slower on the same row. Reused doubled-word capture
searches were 4.4% slower, and the default small-store title first search was
4.3% slower. These measured losses remain in the results; their cause is not
established by this paired run. Literal controls in unchanged competitors also
shifted substantially, so those apparent gains are not attributed to this patch.
Title seek construction and title construct-plus-search have overlapping ranges
and are inconclusive. Method-call construct-plus-search has disjoint ranges.

Unchanged competitor shifts and shared-workstation noise limit attribution of
small differences. This suite covers sparse hits, misses, dense matching and
literal/class-run controls; it is not an overall engine ranking. Defaults and
tuned seek configurations remain separate. ReSharp appears only where exact
results match, not as a claim of interchangeable semantics for all patterns.

Profiles taken separately from timing identify remaining costs in Unicode
delegate construction, first-use automata setup and dense iterator bookkeeping.
Those are future work, not improvements included in this release.

To reproduce, use separate checkouts at the source commits above. Restore only
the measurement checkout's baseline `benchmarks/` directory from the candidate
so both sides use the same adapter. Run `mise run bench:rebar:verify` on each.
Then execute the per-engine measurement command in the orders in the metadata,
with builds and profiling stopped. Keep the historical baseline separately.

```sh
taskset -c 2 target/release/rebar measure -d rebar -e '^regexrs$' \
  --max-time 200ms --max-warmup-time 50ms --max-iters 10000000
```

## Competitive pass, 2026-10-11

The current tables compare candidate `cd261540d684686a71260bd8c556cd5a9c58e53a`
with frozen baseline `34cf8625ffecff621ccd94b209e8b19ee031b722`.
Both library revisions used the candidate's exact runner, dependency lock,
fixtures, definitions and features, including `regexrs-seek`. The release
changes package metadata and documentation, not measured library code.

The physical host was an AMD Ryzen 9 9950X3D, Linux 7.2.5-3-omarchy, performance
governor, nightly-2026-10-11. Each process was pinned to CPU 2; its SMT sibling
was not reserved. No task builds or profiling ran on the host during timing.
This is a shared workstation, not an isolated benchmark machine.

All 132 exact-result and expected-count gates passed on each revision before
timing. Six repetitions rotated and reversed engine order, alternating the
baseline/candidate order for each pair. This does not fully balance all nine
engine positions. Each row used 50 ms warmup and a 200 ms measurement budget,
up to 10 million iterations. All 1,584 timed rows passed count checks.

Numbers are medians of six per-run medians. Ranges are their minimum and
maximum, not confidence intervals. Matching excludes construction; `compile`
rows measure in-process pattern construction, not Cargo or cold startup.

- [Paired raw measurements](../benchmarks/results/2026-10-11-competitive/measurements.csv).
- [All medians and ranges, in nanoseconds](../benchmarks/results/2026-10-11-competitive/summary.csv).
- [Sources, host, budgets and exact orders](../benchmarks/results/2026-10-11-competitive/metadata.json).

Measured changes:

- `ordinary/rust-keywords` with `regexrs`: 142.42 ms to 11.49 ms, 12.39× as fast.
- `rich/titled-names` with `regexrs`: 13.92 ms to 65.06 µs, 213.94× as fast.
- `rich/rust-method-calls` with `regexrs`: 81.53 ms to 6.31 ms, 12.92× as fast.

Non-overlapping regressions above 5% in the same paired run:

- `edge/case-folds` with `regexrs`: 0.24 µs to 0.26 µs, 8.3% slower.
- `edge/empty-utf8-boundaries` with `regexrs`: 0.38 µs to 0.41 µs, 8.0% slower.

The unchanged regex and Fancy controls also improved by about 32% on the
literal miss and 8–9% on literal hits between the two runner builds. ReSharp
was about 11% slower on exclamations. Those shifts prevent attributing the
literal improvements to regexrs changes; shared code generation or host effects
remain unisolated. The large routing and lookbehind gains above have no similar
control shift. Controls, regressions and overlapping ranges remain in the data. Seek remains opt-in,
not an automatic best choice. ReSharp comparisons cover only patterns whose
results match; its leftmost-longest semantics are not interchangeable with all
regexrs patterns. Neither these gains nor the selected README rows establish
an overall engine ranking.

To reproduce the pair, prepare separate checkouts at the two recorded commits.
In the baseline measurement checkout only, restore `benchmarks/` from the
candidate commit so both use the same adapter. Build and verify each with
`mise run bench:rebar:verify`, then run the per-engine command below in the
orders recorded in the new metadata. Keep builds and profiling out of timing.
Preserve the historical baseline separately.

## Physical baseline, 2026-10-11

The source was clean at `c08c8022a5b4dad8eb9f0ee9f33e23af6f90db55`, with
nightly-2026-10-11 and the Rebar revision and dependencies pinned above.
The host ran Linux 7.2.5-3-omarchy on an AMD Ryzen 9 9950X3D with the
performance governor. Each process was pinned to logical CPU 2 with `taskset`.
Its SMT sibling, CPU 18, was not reserved. No compilation or profiling ran
during measurement. This is a controlled run on a shared workstation, not a
dedicated isolated benchmark machine.

Every engine ran eight times. Engine positions and immediate predecessors
were balanced across the eight orders recorded in
[metadata.json](../benchmarks/results/2026-10-11/metadata.json).
Each row had a 50 ms warmup and a 200 ms measurement budget, with an iteration
cap of 10 million. Matching rows exclude pattern construction and initial
native compilation. The `compile` rows time in-process pattern construction,
not Cargo builds or cold process startup.

All 112 exact-result gates passed before measurement. All 896 timed rows
passed their expected-count checks. The figures are the median of eight
per-run medians; the reported range is the minimum and maximum of those
eight medians, not a confidence interval or the spread of individual searches.

- [Raw measurements](../benchmarks/results/2026-10-11/measurements.csv).
- [Every row's median and range, in nanoseconds](../benchmarks/results/2026-10-11/summary.csv).
- [Sources, binary identity, host conditions, budgets and engine orders](../benchmarks/results/2026-10-11/metadata.json).

The historical dataset below predates the current README tables. Findings at
that revision included:

- Default `Regex` took 140.92 ms for `ordinary/rust-keywords`, against 9.98 ms
  for regex. At that revision those word boundaries ran on the rich VM: an
  inherited routing cost, not evidence that automata are slower. Later
  revisions route such patterns to automata; the paired competitive pass above measures
  the later revision.
- `rich/titled-names` took 13.68 ms with default `Regex`, 1.32 ms with
  Fancy's opt-in seek prefilter, and 25.34 µs with ReSharp. Default and tuned
  configurations must stay separate; default-only comparisons hide this gap.
- `regular/bounded-words` took 5.47 ms with `RegularRegex` and 11.25 ms with
  `Regex`. But `regular/log-extract` took 638 µs versus 177 µs. Disabling
  bounded backtracking moves some capture work to the PikeVM.
- `scratch/log-records` took 174.61 µs through retained `Searcher` locations,
  175.01 µs through owned `Regex` captures, and 174.10 µs through the regex
  crate's retained locations. Their ranges overlap. Reusing scratch is not a
  demonstrated speedup here, and this suite does not count allocations.

To repeat the measurement after `mise run bench:rebar:verify`, run each engine
separately in the recorded orders, saving each CSV:

```sh
cd benchmarks
taskset -c 2 target/release/rebar measure -d rebar -e '^regexrs$' \
  --max-time 200ms --max-warmup-time 50ms --max-iters 10000000 > regexrs.csv
```

Use an available physical core on your own host. Repeat for every engine name
in the suite and aggregate medians within each row; do not combine unrelated workloads
or use this host's timings as universal expectations.
