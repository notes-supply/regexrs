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

Native compilation currently accelerates boolean matching on Linux x86-64.
Captures, iteration, replacement and unsupported patterns remain fully available
through the portable implementation. Some expressions favor competing engines;
ordinary matching is not inherently faster than `regex-automata`, and enabling
Fancy's existing seek option changes many apparent gaps. Compare configured
engines, hit-heavy and miss-heavy inputs, and compilation cost separately.

Repeated alternatives such as `((a|ab)+)\1` currently defer to portable execution
and are a known performance weak spot. The existing `RegexBuilder::seek(true)`
option is available for explicit workload tuning, but is not selected
automatically: it can regress other, hit-heavy searches.

Caller-owned scratch is an allocation/ownership interface, not a guaranteed
speedup: the portable implementation already pools working memory. Owned capture
results still allocate independent snapshots. No results or haystacks are cached.
