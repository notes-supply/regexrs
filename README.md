# regexrs

A pure-Rust engine for regular expressions with backreferences, lookarounds,
captures, replacement, Unicode and byte-oriented matching.

```sh
cargo add regexrs
```

```rust
use regexrs::Regex;

let re = Regex::new(r"(\w+)\s+\1")?;
assert!(re.is_match("word word")?);
```

`Regex` supports regular and rich patterns. Matching is fallible: execution
limits return errors, not false nonmatches. It is not fully PCRE-compatible.
[API documentation](https://docs.rs/regexrs) covers the syntax and limits.

For repeated searches, `re.searcher()` reuses working memory, and `captures_read`
fills caller-owned `CaptureLocations`. This reuses storage, not cached answers.
Owned captures remain snapshots, unaffected by later searches.

`RegularRegex` accepts only the regular subset of the syntax and never
backtracks; its searches cannot fail.

## Benchmarks

The tables use the **2026-10-15 hosted comparison** of merged engine source
[`ced8f9e`](https://github.com/notes-supply/regexrs/commit/ced8f9ed44e23617a0ea2b0cc694d694b251fe64).
All cells come from the same run on an AMD EPYC 7763 GitHub-hosted VM, using
nightly-2026-10-11 and five CPU-pinned passes with rotated engine and revision
order. Results are relative observations on that host, not an overall engine
ranking or a comparison with the older Ryzen measurements.

In the engine tables, each cell shows the median of five run medians and its
ratio to the lowest median in that row. **Lower is better; 2.00× takes twice as long.** Bold requires
a run-median range below every other compared engine's range. Overlapping
ranges are unbolded; ranges are not confidence intervals. Matching excludes
pattern construction unless the row says otherwise.

Versions: regexrs source `ced8f9e`, regex 1.13.1, fancy-regex 0.19.2 and
ReSharp 0.7.5. Defaults unless labeled otherwise. ReSharp uses full Unicode
classes and text-boundary anchors. A dash means not compared, not zero cost.
Every compared row passed exact-result checks before timing.

### Changes from previous main

These default-`Regex` operations improved with non-overlapping run-median
ranges against previous main `17ef31b` in the same five-pass run. Percentages
are the median of the five paired time reductions.

| Workload | Previous main | Merged source | Time reduction |
|---|---:|---:|---:|
| Multiline word matching | 6.10 ms | 462.98 µs | 92.4% |
| Log-time captures | 143.16 µs | 30.07 µs | 79.0% |
| Dense-name replacement | 40.77 ms | 1.70 ms | 95.8% |
| Dense captured replacement | 25.11 ms | 2.46 ms | 90.2% |
| Title-lookbehind construction | 310.85 µs | 31.90 µs | 89.7% |
| Title construction + first search | 337.95 µs | 39.00 µs | 88.5% |

The full comparison also retains nine non-overlapping regressions above 5%,
with a largest paired increase of 8.2%. [Ranges, losses and reproduction
steps](docs/benchmarks.md#execution-selection-comparison-2026-10-15) accompany
all 5,290 measurements. This selected table is not a claim that every operation
improved.

### Regular patterns

Whole-corpus matching on books, subtitles and Rust source. The keyword row
searches each line separately. regexrs uses its default `Regex`, not `RegularRegex`.
[Patterns and workloads](benchmarks/rebar/definitions/ordinary.toml).

| Workload | regexrs | regexrs + seek | regex | Fancy | ReSharp |
|---|---:|---:|---:|---:|---:|
| `Sherlock Holmes` | 18.41 µs (1.05×) | 18.37 µs (1.04×) | **17.58 µs (1.00×)** | 18.17 µs (1.03×) | 32.54 µs (1.85×) |
| `Sherlock Holmes` (ignore case) | 61.45 µs (1.34×) | 61.16 µs (1.34×) | 65.20 µs (1.43×) | 66.81 µs (1.46×) | **45.71 µs (1.00×)** |
| `zqxjkw` (no matches) | 14.87 µs (1.01×) | 14.85 µs (1.01×) | 14.82 µs (1.01×) | 14.70 µs (1.00×) | 27.71 µs (1.89×) |
| Names (alternation) | 79.94 µs (1.20×) | 79.79 µs (1.19×) | 78.30 µs (1.17×) | 80.29 µs (1.20×) | **66.82 µs (1.00×)** |
| `\w+` (English) | 3.56 ms (1.00×) | 3.56 ms (1.00×) | 8.41 ms (2.36×) | 9.07 ms (2.55×) | 6.07 ms (1.71×) |
| `"[^"]*"` (dialogue) | 150.76 µs (1.01×) | 151.14 µs (1.01×) | 1.34 ms (8.95×) | 1.36 ms (9.08×) | 149.77 µs (1.00×) |
| `\w+` (Cyrillic) | 7.13 ms (1.00×) | 7.13 ms (1.00×) | 15.25 ms (2.14×) | 15.96 ms (2.24×) | 12.00 ms (1.68×) |
| Rust keyword lines | 20.00 ms (1.10×) | 19.97 ms (1.10×) | **18.12 ms (1.00×)** | 306.33 ms (16.91×) | 28.99 ms (1.60×) |

Word-boundary patterns use automata when their results match the VM.
Dialogue ranges overlap between regexrs and ReSharp; ReSharp has the lowest
non-overlapping range on the name-alternation row.

### Backreferences and lookaround

regexrs uses its portable engine. Both engines' opt-in seek prefilters have their own
columns. The three-word miss is measured per line; its whole-corpus search
exceeds the default backtracking limits in both regexrs and Fancy.
[Patterns and workloads](benchmarks/rebar/definitions/rich.toml).

| Workload | regexrs | regexrs + seek | Fancy | Fancy + seek | ReSharp |
|---|---:|---:|---:|---:|---:|
| Repeated words | 42.93 ms (1.32×) | **32.49 ms (1.00×)** | 51.87 ms (1.60×) | 36.76 ms (1.13×) | — |
| Tripled words (per-line miss) | 40.93 ms (1.68×) | **24.38 ms (1.00×)** | 46.51 ms (1.91×) | 25.72 ms (1.05×) | — |
| Names after `Mr.` or `Dr.` | 74.29 µs (1.46×) | 74.35 µs (1.46×) | 29.34 ms (576.54×) | 2.50 ms (49.13×) | **50.89 µs (1.00×)** |
| Words before `!` | 35.00 µs (1.00×) | 34.95 µs (1.00×) | 54.72 µs (1.57×) | 54.53 µs (1.56×) | 42.39 µs (1.21×) |
| Rust raw strings | 23.41 ms (1.22×) | 19.35 ms (1.01×) | 178.24 ms (9.32×) | 19.12 ms (1.00×) | — |
| Rust method calls | 4.61 ms (1.00×) | 4.60 ms (1.00×) | 258.78 ms (56.26×) | 33.96 ms (7.38×) | 5.40 ms (1.17×) |

### Native execution

The optional `native` feature accelerates supported boolean queries on Linux
x86-64. This row searches for repeated words in each line of the same book.
It is not a measurement of captures or whole-corpus match iteration.

| Workload | regexrs, portable | regexrs, native | regexrs + seek | Fancy | Fancy + seek |
|---|---:|---:|---:|---:|---:|
| Repeated words (per line) | 43.32 ms (3.51×) | **12.33 ms (1.00×)** | 30.95 ms (2.51×) | 50.61 ms (4.10×) | 34.78 ms (2.82×) |

Unsupported queries run portably. Native work is bounded, and explicit
backtracking limits disable native execution. No C regex library or fallback
is used. See [performance and scope](docs/performance.md).

### Backtracking-free searches

`RegularRegex` makes searches infallible by rejecting rich syntax and avoiding
backtracking. It is not always faster. Both APIs below return owned snapshots
on the capture rows. [Patterns and workloads](benchmarks/rebar/definitions/regular.toml).

| Workload | `Regex` | `RegularRegex` |
|---|---:|---:|
| `Sherlock Holmes` | 18.43 µs (1.03×) | **17.98 µs (1.00×)** |
| `\b\w+\b` | 6.18 ms (1.17×) | **5.30 ms (1.00×)** |
| Rust function captures | 1.99 ms (1.03×) | 1.93 ms (1.00×) |
| Log captures (per line) | **413.57 µs (1.00×)** | 1.37 ms (3.31×) |

These are selected workloads, not an overall ranking.
[All measurements, ranges and reproduction steps](docs/benchmarks.md#execution-selection-comparison-2026-10-15)
include pattern compilation, additional capture workloads and reused storage.

## Development

`mise install --locked`, then `mise run check`.
Run the cross-engine suite with `mise run bench:rebar:measure`; see the
[benchmark guide](docs/benchmarks.md). Benchmark tools and reference engines
are development-only.

Independently maintained derivative of Fancy Regex 0.19.1. See [NOTICE](NOTICE)
for provenance and [LICENSE](LICENSE) for the MIT license.
