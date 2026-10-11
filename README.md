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

Measured on an AMD Ryzen 9 9950X3D with nightly-2026-10-11, using eight
CPU-pinned runs with rotated engine order. Each result is the median of the
eight run medians. Matching excludes pattern construction.

Each cell shows elapsed time and its ratio to the fastest engine in that row:
**lower is better; 2.00× takes twice as long**. Bold marks the lowest measured
median. Ratios are rounded, and small differences can be noise.

Versions: regexrs 0.1.3, regex 1.13.1, fancy-regex 0.19.2, ReSharp 0.7.5.
Defaults unless labeled otherwise; ReSharp uses full Unicode classes and
text-boundary anchors. A dash means not compared, not a zero-cost result.
Every compared row passed exact-result checks before timing.

### Regular patterns

Whole-corpus matching on books, subtitles and Rust source. The keyword row
searches each line separately. regexrs uses its default `Regex`, not `RegularRegex`.
[Patterns and workloads](benchmarks/rebar/definitions/ordinary.toml).

| Workload | regexrs | regex | Fancy | ReSharp |
|---|---:|---:|---:|---:|
| `Sherlock Holmes` | 9.49 µs (1.02×) | **9.26 µs (1.00×)** | 9.56 µs (1.03×) | 11.60 µs (1.25×) |
| `Sherlock Holmes` (ignore case) | 34.29 µs (1.45×) | 34.08 µs (1.44×) | 34.49 µs (1.46×) | **23.61 µs (1.00×)** |
| `zqxjkw` (no matches) | **7.02 µs (1.00×)** | 7.03 µs (1.00×) | **7.02 µs (1.00×)** | 10.81 µs (1.54×) |
| Names (alternation) | 43.78 µs (1.57×) | 41.35 µs (1.48×) | 41.76 µs (1.49×) | **27.94 µs (1.00×)** |
| `\w+` (English) | 5.34 ms (1.48×) | 4.98 ms (1.38×) | 5.20 ms (1.44×) | **3.61 ms (1.00×)** |
| `"[^"]*"` (dialogue) | 746.80 µs (11.29×) | 734.96 µs (11.11×) | 748.51 µs (11.32×) | **66.15 µs (1.00×)** |
| `\w+` (Cyrillic) | 9.09 ms (1.31×) | 8.48 ms (1.22×) | 8.82 ms (1.27×) | **6.96 ms (1.00×)** |
| Rust keyword lines | 140.92 ms (14.12×) | **9.98 ms (1.00×)** | 139.82 ms (14.01×) | 14.84 ms (1.49×) |

The keyword row was measured while `Regex` ran word-boundary patterns on its
backtracking engine. They now use automata where the results are identical;
this row has not been measured since. ReSharp is particularly strong on
dialogue and name searches here.

### Backreferences and lookaround

regexrs uses its portable engine. Fancy's opt-in seek prefilter has its own
column. The three-word miss is measured per line; its whole-corpus search
exceeds the default backtracking limits in both regexrs and Fancy.
[Patterns and workloads](benchmarks/rebar/definitions/rich.toml).

| Workload | regexrs | Fancy | Fancy + seek | ReSharp |
|---|---:|---:|---:|---:|
| Repeated words | 20.74 ms (1.07×) | 25.86 ms (1.33×) | **19.42 ms (1.00×)** | — |
| Tripled words (per-line miss) | 19.51 ms (1.45×) | 22.92 ms (1.70×) | **13.50 ms (1.00×)** | — |
| Names after `Mr.` or `Dr.` | 13.68 ms (539.86×) | 13.44 ms (530.39×) | 1.32 ms (51.89×) | **25.34 µs (1.00×)** |
| Words before `!` | 25.70 µs (1.39×) | 25.49 µs (1.38×) | 25.63 µs (1.39×) | **18.45 µs (1.00×)** |
| Rust raw strings | 10.95 ms (1.09×) | 75.39 ms (7.49×) | **10.07 ms (1.00×)** | — |
| Rust method calls | 78.63 ms (24.84×) | 113.11 ms (35.74×) | 17.48 ms (5.52×) | **3.17 ms (1.00×)** |

### Native execution

The optional `native` feature accelerates supported boolean queries on Linux
x86-64. This row searches for repeated words in each line of the same book.
It is not a measurement of captures or whole-corpus match iteration.

| Workload | regexrs, portable | regexrs, native | Fancy | Fancy + seek |
|---|---:|---:|---:|---:|
| Repeated words (per line) | 20.58 ms (3.52×) | **5.85 ms (1.00×)** | 25.40 ms (4.34×) | 18.27 ms (3.12×) |

Unsupported queries run portably. Native work is bounded, and explicit
backtracking limits disable native execution. No C regex library or fallback
is used. See [performance and scope](docs/performance.md).

### Backtracking-free searches

`RegularRegex` makes searches infallible by rejecting rich syntax and avoiding
backtracking. It is not always faster. Both APIs below return owned snapshots
on the capture rows. [Patterns and workloads](benchmarks/rebar/definitions/regular.toml).

| Workload | `Regex` | `RegularRegex` |
|---|---:|---:|
| `Sherlock Holmes` | **9.51 µs (1.00×)** | 9.53 µs (1.00×) |
| `\b\w+\b` | 11.25 ms (2.06×) | **5.47 ms (1.00×)** |
| Rust function captures | **3.04 ms (1.00×)** | 5.05 ms (1.66×) |
| Log captures (per line) | **177.37 µs (1.00×)** | 637.90 µs (3.60×) |

These are selected workloads, not an overall ranking.
[All measurements, ranges and reproduction steps](docs/benchmarks.md#physical-baseline-2026-10-11)
include pattern compilation, additional capture workloads and reused storage.

## Development

`mise install --locked`, then `mise run check`.
Run the cross-engine suite with `mise run bench:rebar:measure`; see the
[benchmark guide](docs/benchmarks.md). Benchmark tools and reference engines
are development-only.

Independently maintained derivative of Fancy Regex 0.19.1. See [NOTICE](NOTICE)
for provenance and [LICENSE](LICENSE) for the MIT license.
