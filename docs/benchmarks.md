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

Capture APIs differ by engine, and each row uses the engine's API as listed:

- **Reusable storage:** `regexrs-searcher` (`Searcher::captures_read` into one
  `CaptureLocations`) and `regex` (`Regex::captures_read` into one
  `CaptureLocations`). Both create their storage once, outside the timed loop,
  and reuse it across records and iterations.
- **Owned snapshots:** `regexrs`, `regexrs-regular` and `fancy` return an owned
  `Captures` per record.

Compare reusable rows with reusable rows, and snapshot rows with snapshot rows.
A gap between the two kinds reflects the API choice, not engine speed.

## Engines and configurations

| engine | configuration |
|---|---|
| `regexrs` | default `Regex`, native code disabled (the default build) |
| `regexrs-native` | `Regex` with the `native` feature; boolean searches only |
| `regexrs-searcher` | `Regex` through one reused `Searcher` and `CaptureLocations` |
| `regexrs-regular` | `RegularRegex`: no backtracking, regex-automata's bounded backtracker disabled |
| `regex` | regex 1.13.1, defaults |
| `fancy` | fancy-regex 0.19.2, defaults |
| `fancy-seek` | fancy-regex with its opt-in seek prefilter |
| `resharp` | resharp 0.7.5, full Unicode classes, `^`/`$` at text boundaries |

Defaults and tuned configurations are separate engines and must not be merged
into one result. resharp is leftmost-longest: it is listed only on rows whose
patterns avoid its syntax extensions (`_`, `&`, `~`) and where verification
shows the same spans as leftmost-first engines. Where it differs, the row fails
verification instead of being compared.

## Definitions

All rows use Unicode mode.

- `ordinary.toml`: regular patterns on real text, including literal hits,
  case-insensitive hits, a selective miss, a name alternation, dense words,
  dialogue spans, Cyrillic words, line-oriented keyword search, captured
  function names and the real-world log extraction regex.
- `rich.toml`: backreferences and lookaround. These include doubled words
  (whole text and per line), a selective per-line miss, names after titles,
  words before `!`, Rust raw strings and method calls.
  A whole-text search for the miss `\b(\w+) \1 \1\b` exceeds the default
  backtrack limit in regexrs and fancy-regex alike, so that row is per line.
- `regular.toml`: `RegularRegex` against `Regex` and the regex crate.
- `scratch.toml`: `Searcher` and `CaptureLocations` against plain calls.
- `edge.toml`: small checked-in fixtures for empty matches at UTF-8
  boundaries, unmatched versus empty groups, and case folds of different
  UTF-8 widths.

The suite has 33 definitions and 112 engine rows.

### Provenance

- From the pinned Rebar checkout, not redistributed here:
  - `sherlock.txt`, *The Adventures of Sherlock Holmes* from Project Gutenberg, public domain.
  - `opensubtitles/ru-sampled.txt`, a sample of the OPUS OpenSubtitles 2018 corpus.
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

The README selects six rows from this data. Important findings outside that
excerpt include:

- Default `Regex` took 140.92 ms for `ordinary/rust-keywords`, against 9.98 ms
  for regex. Those word boundaries still run on the rich VM. This is an
  inherited routing cost, not evidence that automata are slower.
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

Use an available physical core on your own host. Repeat for all eight engine
names and aggregate medians within each row; do not combine unrelated workloads
or use this host's timings as universal expectations.
