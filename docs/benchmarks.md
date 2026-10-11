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
