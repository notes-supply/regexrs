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

Independently maintained derivative of Fancy Regex 0.19.1. See [NOTICE](NOTICE)
for provenance and [LICENSE](LICENSE) for the MIT license.
