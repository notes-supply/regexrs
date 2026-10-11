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

Measured on an AMD Ryzen 9 9950X3D with nightly-2026-10-11, using six
CPU-pinned paired runs with rotated engine order. Each result is the median of the
six candidate run medians. Matching excludes pattern construction.

Each cell shows elapsed time and its ratio to the fastest engine in that row:
**lower is better; 2.00× takes twice as long**. Bold marks the lowest measured
median. Ratios are rounded, and small differences can be noise.

Versions: regexrs source for 0.1.4 (`cd26154`), regex 1.13.1,
fancy-regex 0.19.2, ReSharp 0.7.5.
Defaults unless labeled otherwise; ReSharp uses full Unicode classes and
text-boundary anchors. A dash means not compared, not a zero-cost result.
Every compared row passed exact-result checks before timing.

### Regular patterns

Whole-corpus matching on books, subtitles and Rust source. The keyword row
searches each line separately. regexrs uses its default `Regex`, not `RegularRegex`.
[Patterns and workloads](benchmarks/rebar/definitions/ordinary.toml).

| Workload | regexrs | regexrs + seek | regex | Fancy | ReSharp |
|---|---:|---:|---:|---:|---:|
| `Sherlock Holmes` | 9.56 µs (1.04×) | 9.64 µs (1.05×) | **9.21 µs (1.00×)** | 9.36 µs (1.02×) | 11.38 µs (1.24×) |
| `Sherlock Holmes` (ignore case) | 34.78 µs (1.48×) | 34.71 µs (1.47×) | 34.07 µs (1.45×) | 34.34 µs (1.46×) | **23.56 µs (1.00×)** |
| `zqxjkw` (no matches) | 6.96 µs (1.00×) | 6.97 µs (1.00×) | **6.95 µs (1.00×)** | 6.96 µs (1.00×) | 10.86 µs (1.56×) |
| Names (alternation) | 44.52 µs (1.57×) | 44.41 µs (1.56×) | 41.20 µs (1.45×) | 41.30 µs (1.46×) | **28.39 µs (1.00×)** |
| `\w+` (English) | 5.71 ms (1.54×) | 5.62 ms (1.52×) | 4.79 ms (1.29×) | 5.19 ms (1.40×) | **3.71 ms (1.00×)** |
| `"[^"]*"` (dialogue) | 768.36 µs (12.14×) | 764.13 µs (12.07×) | 735.82 µs (11.63×) | 750.20 µs (11.85×) | **63.29 µs (1.00×)** |
| `\w+` (Cyrillic) | 9.48 ms (1.34×) | 9.37 ms (1.32×) | 8.51 ms (1.20×) | 8.99 ms (1.27×) | **7.10 ms (1.00×)** |
| Rust keyword lines | 11.49 ms (1.16×) | 11.55 ms (1.17×) | **9.88 ms (1.00×)** | 140.63 ms (14.23×) | 14.72 ms (1.49×) |

Word-boundary patterns use automata when their results match the VM.
ReSharp remains particularly strong on dialogue and name searches here.

### Backreferences and lookaround

regexrs uses its portable engine. Both engines' opt-in seek prefilters have their own
columns. The three-word miss is measured per line; its whole-corpus search
exceeds the default backtracking limits in both regexrs and Fancy.
[Patterns and workloads](benchmarks/rebar/definitions/rich.toml).

| Workload | regexrs | regexrs + seek | Fancy | Fancy + seek | ReSharp |
|---|---:|---:|---:|---:|---:|
| Repeated words | 21.23 ms (1.28×) | **16.54 ms (1.00×)** | 26.41 ms (1.60×) | 19.21 ms (1.16×) | — |
| Tripled words (per-line miss) | 19.80 ms (1.61×) | **12.33 ms (1.00×)** | 23.34 ms (1.89×) | 13.31 ms (1.08×) | — |
| Names after `Mr.` or `Dr.` | 65.06 µs (2.57×) | 1.31 ms (51.68×) | 13.59 ms (536.09×) | 1.31 ms (51.68×) | **25.35 µs (1.00×)** |
| Words before `!` | 26.14 µs (1.28×) | 26.08 µs (1.27×) | 25.14 µs (1.23×) | 25.22 µs (1.23×) | **20.48 µs (1.00×)** |
| Rust raw strings | 11.10 ms (1.10×) | **10.04 ms (1.00×)** | 75.83 ms (7.55×) | 10.08 ms (1.00×) | — |
| Rust method calls | 6.31 ms (1.96×) | 16.89 ms (5.25×) | 114.18 ms (35.46×) | 17.51 ms (5.44×) | **3.22 ms (1.00×)** |

### Native execution

The optional `native` feature accelerates supported boolean queries on Linux
x86-64. This row searches for repeated words in each line of the same book.
It is not a measurement of captures or whole-corpus match iteration.

| Workload | regexrs, portable | regexrs, native | regexrs + seek | Fancy | Fancy + seek |
|---|---:|---:|---:|---:|---:|
| Repeated words (per line) | 21.17 ms (3.65×) | **5.80 ms (1.00×)** | 15.69 ms (2.70×) | 25.36 ms (4.37×) | 18.14 ms (3.13×) |

Unsupported queries run portably. Native work is bounded, and explicit
backtracking limits disable native execution. No C regex library or fallback
is used. See [performance and scope](docs/performance.md).

### Backtracking-free searches

`RegularRegex` makes searches infallible by rejecting rich syntax and avoiding
backtracking. It is not always faster. Both APIs below return owned snapshots
on the capture rows. [Patterns and workloads](benchmarks/rebar/definitions/regular.toml).

| Workload | `Regex` | `RegularRegex` |
|---|---:|---:|
| `Sherlock Holmes` | 9.72 µs (1.05×) | **9.30 µs (1.00×)** |
| `\b\w+\b` | **4.70 ms (1.00×)** | 5.13 ms (1.09×) |
| Rust function captures | **3.08 ms (1.00×)** | 5.00 ms (1.62×) |
| Log captures (per line) | **178.47 µs (1.00×)** | 626.18 µs (3.51×) |

These are selected workloads, not an overall ranking.
[All measurements, ranges and reproduction steps](docs/benchmarks.md#competitive-pass-2026-10-11)
include pattern compilation, additional capture workloads and reused storage.

## Development

`mise install --locked`, then `mise run check`.
Run the cross-engine suite with `mise run bench:rebar:measure`; see the
[benchmark guide](docs/benchmarks.md). Benchmark tools and reference engines
are development-only.

Independently maintained derivative of Fancy Regex 0.19.1. See [NOTICE](NOTICE)
for provenance and [LICENSE](LICENSE) for the MIT license.
