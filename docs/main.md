An implementation of regexes, supporting a relatively rich set of features, including backreferences
and lookaround. Aims to be compatible with Oniguruma syntax when the relevant flag is set.

It builds on top of the excellent [regex] crate. If you are not
familiar with it, make sure you read its documentation and ordinary patterns may not need regexrs.

Ordinary expressions are delegated to `regex-automata`, which provides linear-time
matching. Rich expressions retain a portable backtracking implementation. The
optional `native` feature accelerates supported whole-string boolean searches on
Linux x86-64; other operations and expressions remain portable. Backtracking can
still take exponential time, depending on the expression and input. Execution
limits return errors rather than silently reporting a nonmatch.

# Usage

The API should feel very similar to the regex crate, and involves compiling a regex and then using
it to find matches in text.

## Example: Matching text

An example with backreferences to check if a text consists of two identical words:

```rust
use regexrs::Regex;

let re = Regex::new(r"^(\w+) (\1)$").unwrap();
let result = re.is_match("foo foo");

assert!(result.is_ok());
let did_match = result.unwrap();
assert!(did_match);
```

Note that like in the regex crate, the regex needs anchors like `^` and `$` to match against the
entire input text.

## Example: Finding the position of matches

```rust
use regexrs::Regex;

let re = Regex::new(r"(\d)\1").unwrap();
let result = re.find("foo 22");

assert!(result.is_ok(), "execution was successful");
let match_option = result.unwrap();

assert!(match_option.is_some(), "found a match");
let m = match_option.unwrap();

assert_eq!(m.start(), 4);
assert_eq!(m.end(), 6);
assert_eq!(m.as_str(), "22");
```

## Example: Capturing groups

```rust
use regexrs::Regex;

let re = Regex::new(r"(?<!AU)\$(\d+)").unwrap();
let result = re.captures("AU$10, $20");

let captures = result.expect("Error running regex").expect("No match found");
let group = captures.get(1).expect("No group");
assert_eq!(group.as_str(), "20");
```

## Example: Searching within a range without slicing

```rust
use regexrs::{Regex, RegexInput};

let re = Regex::new(r"\bat\b").unwrap();
let haystack = "batter at";

assert!(re.find("at").unwrap().is_some());
assert!(re
    .find_input(RegexInput::new(haystack).range(1..3))
    .unwrap()
    .is_none());
```

Using [`RegexInput`] preserves the original haystack for anchors, word
boundaries, and lookaround while still constraining the reported match to a
specific byte range.

## RegexSet notes for `regex` crate users

`regexrs::RegexSet` is not a drop-in API replacement for `regex::RegexSet`.

- `regex::RegexSet` answers "which patterns matched anywhere?"
- `regexrs::RegexSet` answers "what matched at the earliest position?" and yields
  full matches (with offsets and captures) in pattern index order.
- Each yielded match is a `Result`, since fancy features may fail at runtime (for
  example when a backtracking limit is exceeded).

This design is useful for tasks like tokenization/syntax-highlighting where you
need both match locations and explicit control over priority resolution.

## Example: Splitting text

```rust
use regexrs::Regex;

let re = Regex::new(r"[ \t]+").unwrap();
let target = "a b \t  c\td    e";
let fields: Vec<&str> = re.split(target).map(|x| x.unwrap()).collect();
assert_eq!(fields, vec!["a", "b", "c", "d", "e"]);

let fields: Vec<&str> = re.splitn(target, 3).map(|x| x.unwrap()).collect();
assert_eq!(fields, vec!["a", "b", "c\td    e"]);
```

[regex]: https://crates.io/crates/regex
