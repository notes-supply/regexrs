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

Regular patterns whose every choice the next character decides are searched
by a one-pass plan instead of an automaton. A plan is a program of literals,
character runs, ordered alternatives and optional or repeated groups, with
captures anywhere. It is admitted where the characters that can start each
alternative, each further run character and each further repetition differ
from those that can follow instead, and no alternative or repeated group can
match nothing. Assertions may stand only at the ends; assertions after a run
must fail wherever the run could stop early. Examples are `\w+`, `"[^"]*"`,
`\bfn\s+(\w+)`, `^\d+$`, `[0-9a-f]{8}-[0-9a-f]{4}`, `Holmes|Watson|Lestrade`,
`\b(?:fn|let|mut)\b`, `(a)?(b*)`, `(\d+)(?:\.(\d+))?` and `\w+(?:\.\w+)*`.

An attempt reads forward only and never returns to an earlier position or
alternative. Where the rest of the pattern could match nothing, a lazy
repetition stops; a greedy one continues after recording the position, with
the capture slots that what follows could change, as the match to report if it
cannot continue. A one-pass DFA keeps its last match the same way. No search
through a plan is handed to another engine. Attempts may fail only after
reading a bounded number of bytes where other attempts could start; an
unbounded first run, which a failed attempt skips, and runs or repetitions
holding no character a match can start with do not count. Work therefore
stays linear per search and per iteration. Patterns such as `(?:foo|bar)+qux`,
whose attempts could rescan unbounded repetitions, keep their engine.

The plan finds candidates with memmem, a literal at a fixed offset, a
`regex-automata` prefilter, memchr or a word-at-a-time byte-range test. A
literal after an unbounded first run that the run cannot take, as `@` in
`\w+@\w+`, gives the candidates instead: at each of its occurrences the plan
scans back over the run to the match start. So does a literal after a fixed
number of characters wider than a byte, as `:` in `(\d{2}):(\d{2})` with
Unicode digits: each occurrence is matched backwards to where a match would
start, in valid UTF-8, while byte haystacks scan for the first character. A
leading multi-line `^` whose first character would otherwise be found by
testing every byte takes line starts, found with memchr, as candidates. A pattern that is a finite set of
literals without captures or assertions is searched with its prefilter alone,
as the meta engine's literal strategy does, whatever the shape of its
alternatives, so `ab|ac` and `(?:a|)b` need no program. Alternatives are chosen
through a 256-entry table of first bytes. Runs match
maximally (or minimally for a lazy run nothing can fail after), and a failed
attempt skips past a first run that cannot match. Captures, ranges, anchoring,
`Searcher` and `CaptureLocations` give the meta engine's results with no
reverse search, cache or pool.

`Regex` and `RegularRegex` then keep no meta engine for the pattern. The engine
is still built once and dropped under explicit size limits, or when a bound
over every part of the pattern (literal bytes, classes, repetitions, capture
groups and assertions) exceeds a budget. Within that budget a pattern needs at
most about 540 KiB of the default 10 MiB NFA size limit. Construction therefore
accepts and rejects the same patterns, with the same errors.

Patterns whose only construct beyond the regular subset is a word boundary
(`\b`, `\B`, `\<`, `\>` and the `\b{...}` forms) are compiled for the portable
VM and also as `regex-automata` lazy DFAs. The DFAs answer a search when they
provably give the VM's result: on text, from a character boundary, with a range
that ends at the end of the haystack, or in ASCII bytes mode. With Unicode word
boundaries they give up at non-ASCII bytes they would need to classify. That
search then runs on native code, for a whole-text boolean, or on the VM. If the
VM exceeds the default backtrack limit there, the automata answer instead,
since they never backtrack; other VM errors, such as a backtrack stack overflow,
are still reported. Rich constructs and options, capture-group-zero rewrites
such as `\K`, byte haystacks outside ASCII mode and an explicit
`backtrack_limit` keep the VM alone. So does an
unbounded repetition the VM runs over an expression that can match empty, such
as `(\b\w*)*`: the VM ends it after an empty iteration, as backtracking engines
do, and automata can report other captures or spans. The automata are built on
the first search that uses them, which costs roughly 0.03–0.3 ms per pattern.
A word-boundary pattern with a plan uses it in place of the automata for
every search on text, including non-ASCII text where the lazy DFAs would give
up and leave the search to the VM. Unicode word boundaries there are the same
assertions the VM tests. Routes exist only under the default backtrack limit,
whose limit errors the route already recovered. Only an unrecovered VM error
on such text, such as a backtrack stack overflow, can therefore become the
correct answer instead. In ASCII mode, where the VM classifies single bytes, a
plan answers only searches that read ASCII alone, and patterns with
alternatives or groups leave those searches to the automata.

A regular pattern between fixed-width lookbehinds at its start and lookaheads
at its end, as `(?<=Mr\. |Dr\. )[A-Z]\w+`, `(?<=\s)\w+`,
`(?<=\.)[a-z][a-z0-9]*(?=\()` or `(?<!\w)\d+`, is searched by a plan as well.
The plan tests each assertion by reading the characters next to the position
it applies to, so the assertion decides no choice. A lookahead is admitted
where the end of the match is already decided: its first characters must not
continue the last step, and a negative one only follows a step of fixed
length. The candidates are where a lookbehind's literals of one byte length
end, as found by a `regex-automata` prefilter. After a failed lookbehind the
plan skips the run of the first step's class, inside which the lookbehind
fails too. A final positive lookahead such as `\w+(?=!)`, which the optimizer
turns into a consumed suffix and a rewritten capture group zero, is planned
on that form. Lookarounds elsewhere, bodies of varying width, captures inside
them, rich options and an explicit `backtrack_limit` keep the VM.

This changes results only where the VM failed under the default limit. A
search whose VM exceeded the default backtrack limit, such as
`(?<=Mr\. )[A-Z]` over a million capitals without `seek`, now returns its
correct answer; that limit only bounded incidental work. An explicit limit
keeps the VM and its exact errors.

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
A seek also uses the start filter. A start filter from a leading lookbehind's
literals, which the seek approximation drops, proposes the candidates and the
seek engine confirms them, anchored; otherwise the seek proposes them and the
start filter rejects those that would fail at their fixed cost, charging that
cost. Both attempt only positions plain seeking attempts. An explicit
`backtrack_limit` keeps the seek in the lead, so its limit errors are exactly
those of seeking alone; under the default limit a lookbehind-led seek can
finish where plain seeking spent the limit on attempts the lookbehind rules out.
The seek approximation is only derived when a seek filter may use it.

Caller-owned scratch is an allocation/ownership interface, not a guaranteed
speedup: the portable implementation already pools working memory. Owned capture
results still allocate independent snapshots. No results or haystacks are cached.

Replacement expands its template in one pass, appending to the output, and
fills one capture buffer for every match. A replacer only borrows each match's
captures, so none can observe a later match's groups.
