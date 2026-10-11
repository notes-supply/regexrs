# regexrs

A pure-Rust engine for regular expressions with backreferences, lookarounds,
captures, replacement, Unicode and byte-oriented matching.

```rust
use regexrs::Regex;

let re = Regex::new(r"(\w+)\s+\1")?;
assert!(re.is_match("word word")?);
```

Ordinary patterns use `regex-automata`; rich patterns retain a portable Rust
implementation, whose unanchored searches skip positions that cannot begin a
match. Matching is fallible: execution limits return errors, not false
nonmatches. This is not a claim of complete PCRE compatibility.

For repeated searches, `let mut search = re.searcher()` reuses working memory
across single searches and iteration, and `captures_read` fills caller-owned
`CaptureLocations`. It does not cache answers. Owned capture results remain
snapshots, unaffected by later searches.

`RegularRegex` accepts only the regular subset of the syntax and never
backtracks; its searches cannot fail.

The optional `native` feature accelerates supported whole-string boolean queries
on Linux x86-64. Other syntax, byte inputs, ranged searches and captures retain
the complete portable engine. Native work is capped; a deferral reruns the input
portably. Explicit backtracking limits disable native execution. No C regex
library or C fallback is used.

Development: `mise install --locked`, then `mise run check`.
Benchmarks: `mise run bench`; see [performance](docs/performance.md) for scope
and other workloads, and [the Rebar suite](docs/benchmarks.md) for verified
cross-engine comparisons. Benchmark tools and reference engines are
development-only.

## Measured baseline

Selected Rebar rows on a Ryzen 9 9950X3D, nightly-2026-10-11. Times are
medians across eight rotated, CPU-pinned runs; lower is better.

| workload | regexrs configuration | time | comparison | time |
|---|---|---:|---|---:|
| literal count | `Regex` | 9.49 µs | regex | 9.26 µs |
| doubled words, per-line boolean | native | 5.85 ms | Fancy, seek enabled | 18.27 ms |
| Rust raw strings | `Regex` | 10.95 ms | Fancy, seek enabled | 10.07 ms |
| Unicode word boundaries | `RegularRegex` | 5.47 ms | regex | 4.94 ms |
| log captures | `RegularRegex` | 638 µs | regex | 177 µs |
| method-call lookaround | `Regex` | 78.63 ms | ReSharp | 3.17 ms |

The literal difference is within run-to-run variation. `RegularRegex` trades
backtracking-free searches for slower captures on some patterns. This is a
curated suite, not an overall ranking. [All results and reproduction details](docs/benchmarks.md#physical-baseline-2026-10-11)
include the remaining wins and losses.

Independently maintained derivative of Fancy Regex 0.19.1. See [NOTICE](NOTICE)
for provenance and [LICENSE](LICENSE) for the MIT license.
