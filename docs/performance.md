# Performance

`mise run bench` compares a synthetic backreference workload against published
Fancy Regex (default and seek-enabled), regexr (JIT and portable), regress and
our native, portable and caller-owned-scratch paths. Every input is checked
against the unmodified Fancy oracle before a configuration is timed. Unsupported
patterns, differing answers and execution errors are reported, not ranked.

The benchmark uses one million short strings in RAM, seven rotated scans and
case-insensitive UTF-8 matching. Scan timings exclude fixture construction, pattern
compilation, file I/O and output. It reports separate warm pattern-build samples;
these are not Cargo compilation timings or cold-process builds. The cached
configuration creates one searcher per timed scan and reuses it across records;
that one-time scratch setup is included.

To select another workload:

```sh
mise exec -- cargo run --release --locked --example competition --features native -- cases
mise exec -- cargo run --release --locked --example competition --features native -- bench 5 1000000 7
```

The first number after `bench` is the zero-based workload index. The next two
are record count and sample count. `conformance` runs a small targeted semantic
comparison, not comprehensive fuzzing. Other engines intentionally use different
regex flavors; disagreement is not automatically a bug.

Native compilation currently accelerates boolean matching on Linux x86-64; its
code is emitted on the first such search. Captures, iteration, replacement and
unsupported patterns remain fully available through the portable
implementation. Some expressions favor competing engines; ordinary matching is
not inherently faster than `regex-automata`, and enabling Fancy's existing seek
option changes many apparent gaps. Compare configured engines, hit-heavy and
miss-heavy inputs, and compilation cost separately.

Patterns whose only construct beyond the regular subset is a word boundary
(`\b`, `\B`, `\<`, `\>` and the `\b{...}` forms) are compiled for the portable
VM and also as `regex-automata` lazy DFAs. The DFAs answer a search when they
provably give the VM's result: on text, from a character boundary, with a range
that ends at the end of the haystack, or in ASCII bytes mode. With Unicode word
boundaries they give up at non-ASCII bytes they would need to classify. That
search then runs on native code, for a whole-text boolean, or on the VM; a
limit error there is answered by the automata instead. Rich constructs and
options, capture-group-zero rewrites such as `\K`, byte haystacks outside
ASCII mode and an explicit `backtrack_limit` keep the VM alone. So does an
unbounded repetition the VM runs over an expression that can match empty, such
as `(\b\w*)*`: the VM ends it after an empty iteration, as backtracking engines
do, and automata can report other captures or spans. The automata are built on
the first search that uses them, which costs roughly 0.03–0.3 ms per pattern.

Unanchored portable searches, including `find`, captures, iteration and
replacement, skip start positions where the program's leading literal or class
tests cannot succeed: memmem or memchr via `regex-automata`, or a first-byte
table. A leading fixed-width lookbehind over a set of literals of one byte
length, as in `(?<=Mr\. |Dr\. )[A-Z]\w+`, instead limits attempts to where one
of its literals ends, found with `regex-automata`'s literal searchers. Each
skipped position is charged the backtracks its failing attempt would have
performed, so matches and limit errors are unchanged. Positions are
only skipped when that cost is fixed, so patterns led by a delegated
alternation, `.`, an assertion before alternatives or another construct whose
failure cost varies still try every position, as do byte haystacks outside
ASCII mode; dense hits gain nothing. Repeated alternatives such as `((a|ab)+)\1` remain portable.
`RegexBuilder::seek(true)` uses a fuller regular approximation for explicit
tuning, but is not selected automatically: it can regress hit-heavy searches.

Caller-owned scratch is an allocation/ownership interface, not a guaranteed
speedup: the portable implementation already pools working memory. Owned capture
results still allocate independent snapshots. No results or haystacks are cached.
