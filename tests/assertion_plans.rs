//! Regular patterns between fixed-width lookbehinds and lookaheads are
//! searched by a plan that tests each assertion next to the position it
//! applies to, and a final lookahead the optimizer turns into a rewritten
//! capture group zero is planned too. Each search API must report what the
//! backtracking VM reports for the same pattern (an explicit backtrack limit
//! keeps every search on it) and what fancy-regex reports.

use regexrs::{Regex, RegexBuilder, RegexInput};

type Groups = Vec<Option<(usize, usize)>>;

/// Patterns that must take a plan, then patterns that must not.
const PLANNED: &[&str] = &[
    r"(?<=Mr\. |Dr\. )[A-Z]\w+",
    r"\w+(?=!)",
    r"(?<=\s)\w+",
    r"(?<=\.)[a-z][a-z0-9]*(?=\()",
    r"(?<!\w)\d+",
    "foo(?!bar)",
    "(?<=a)b+(?=c)",
    "(?<![a-z])[A-Z]+",
    r"(?<=\$)(\d+)(?:\.(\d\d))?",
    r"(?<=[\s(])\w+(?=[)\s])",
    r"(?i)(?<=the )\w+",
    r"(?<=é)\w+",
    r"\b(?<=-)\w+",
    r"(?<=ab|cd)x\d*",
    r"(?<!x)(?<=a)\d",
    // A lookbehind class that the first run can also hold: starts inside the
    // run can pass it after its start fails.
    r"(?<=[a-c])\w+",
];

const UNPLANNED: &[&str] = &[
    // Shorter matches would satisfy the assertion, or it varies in width.
    r"\w+(?!\s)",
    r"(?<=a+)b",
    // Not at an edge, or with captures inside.
    r"(?=a)\w+",
    r"(?<=(a))b",
    r"a(?=b)c",
];

const TEXTS: &[&str] = &[
    "",
    "Mr. Holmes and Dr. Watson met Mrs. Hudson; Mr. x Dr.Y Dr. Émile",
    "wow! so fancy!! yes !no a!b",
    "one two\tthree\nfour (five) [six]",
    "obj.method(x).other().x( y.z(",
    "1 22 a33 444b 5",
    "foobar foo foobaz fo",
    "abbbc abc ac abbb bc",
    "ABC aBC Ab XYZ",
    "$12.50 $7 $3.1 12.00 $$9",
    "(word) (x y) z) (q",
    "The cat THE dog the  end",
    "éabc é x éé1",
    "a-b -cd --e f-",
    "abx1 cdx22 ax3 x4",
    "a1 xa2 a3",
    "xbz qay abc -cd",
];

fn groups(c: &regexrs::Captures<'_>) -> Groups {
    c.iter().map(|m| m.map(|m| (m.start(), m.end()))).collect()
}

/// The VM's answers for a pattern that needs it: an explicit backtrack limit
/// keeps every search there. A pattern the optimizer makes regular, such as
/// one whose only lookaround is final, has no VM; fancy-regex is then the
/// reference its plan answers to.
fn vm_only(pattern: &str) -> Regex {
    RegexBuilder::new(pattern)
        .backtrack_limit(1_000_000)
        .build()
        .unwrap()
}

#[test]
fn assertion_families_take_plans() {
    for pattern in PLANNED {
        assert!(Regex::new(pattern).unwrap().planned(), "{pattern}");
    }
    // Lookbehinds keep the VM, whose reference then takes no route.
    assert!(!vm_only(PLANNED[0]).planned());
    for pattern in UNPLANNED {
        assert!(!Regex::new(pattern).unwrap().planned(), "{pattern}");
    }
}

fn compare(pattern: &str, text: &str) {
    let routed = Regex::new(pattern).unwrap();
    let vm = vm_only(pattern);
    let oracle = oracle::Regex::new(pattern).unwrap();
    let context = format!("{pattern:?} {text:?}");
    let expected: Vec<Groups> = vm
        .captures_iter(text)
        .map(|c| groups(&c.unwrap()))
        .collect();
    let fancy: Vec<Groups> = oracle
        .captures_iter(text)
        .map(|c| {
            let c = c.unwrap();
            c.iter().map(|m| m.map(|m| (m.start(), m.end()))).collect()
        })
        .collect();
    assert_eq!(expected, fancy, "VM against fancy-regex {context}");
    let found: Vec<Groups> = routed
        .captures_iter(text)
        .map(|c| groups(&c.unwrap()))
        .collect();
    assert_eq!(found, expected, "captures_iter {context}");
    let spans: Vec<(usize, usize)> = routed
        .find_iter(text)
        .map(|m| m.map(|m| (m.start(), m.end())).unwrap())
        .collect();
    let expected_spans: Vec<(usize, usize)> = expected.iter().map(|g| g[0].unwrap()).collect();
    assert_eq!(spans, expected_spans, "find_iter {context}");
    assert_eq!(
        routed.is_match(text).unwrap(),
        !expected.is_empty(),
        "is_match {context}"
    );
    let mut search = routed.searcher();
    let mut locations = routed.capture_locations();
    let found = search.captures_read(&mut locations, text).unwrap();
    assert_eq!(
        found.map(|m| (m.start(), m.end())),
        expected_spans.first().copied(),
        "captures_read {context}"
    );
    if let Some(first) = expected.first() {
        let read: Groups = (0..locations.len()).map(|i| locations.get(i)).collect();
        assert_eq!(&read, first, "captures_read {context}");
    }
    assert_eq!(
        routed.replace_all(text, "<$0>"),
        vm.replace_all(text, "<$0>"),
        "replace_all {context}"
    );
    // Later starts and anchors keep the text before them for lookbehinds.
    for start in (0..=text.len()).filter(|&i| text.is_char_boundary(i)) {
        for anchored in [false, true] {
            let input = || RegexInput::new(text).from_pos(start).anchored(anchored);
            let expected = vm.captures_input(input()).unwrap().map(|c| groups(&c));
            let found = routed.captures_input(input()).unwrap().map(|c| groups(&c));
            assert_eq!(
                found, expected,
                "{context} from {start} anchored={anchored}"
            );
            assert_eq!(
                routed.is_match_input(input()).unwrap(),
                expected.is_some(),
                "{context} from {start} anchored={anchored}"
            );
        }
    }
}

#[test]
fn assertion_plans_equal_the_vm_and_fancy_regex() {
    for pattern in PLANNED.iter().chain(UNPLANNED) {
        for text in TEXTS {
            compare(pattern, text);
        }
    }
}

struct Rng(u64);

impl Rng {
    fn below(&mut self, n: usize) -> usize {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 % n as u64) as usize
    }

    fn pick<'a>(&mut self, items: &[&'a str]) -> &'a str {
        items[self.below(items.len())]
    }
}

/// Generated cores between generated edges, including ties the plan must
/// refuse, against the VM and fancy-regex on generated texts.
#[test]
fn generated_assertions_equal_the_vm_and_fancy_regex() {
    let mut rng = Rng(0x2545_F491_4F6C_DD1D);
    const BEHIND: &[&str] = &[
        "",
        "",
        "(?<=a)",
        "(?<=ab|cd)",
        r"(?<=\s)",
        "(?<![a-z])",
        "(?<=é)",
        r"(?<!\d)",
        "(?<=[.-])",
        "(?<=x)(?<!yx)",
        r"\b(?<=-)",
        r"(?<=a)\b",
        "(?<=[a-c])",
    ];
    const CORE: &[&str] = &[
        r"\w+",
        "[a-z]+",
        r"\d+",
        "b+",
        "x",
        r"(\d+)(?:\.(\d+))?",
        "(?:ab|c)+",
        "[A-Z]\\w*",
        "é+",
        "a|bc",
        r"\w+-\w+",
        "k+?",
        "(a)?b",
    ];
    const AHEAD: &[&str] = &[
        "", "", "(?=!)", "(?=c|d)", r"(?=\s)", "(?!x)", "(?=\\()", r"(?!\d)", "(?=é)", r"\b(?!x)",
    ];
    const PIECES: &[&str] = &[
        "a", "b", "c", "d", "x", "y", "é", "1", "2", ".", "-", " ", "!", "(", "ab", "cd", "A", "k",
    ];
    let mut planned = 0;
    let mut compared = 0;
    for _ in 0..1500 {
        let pattern = format!("{}{}{}", rng.pick(BEHIND), rng.pick(CORE), rng.pick(AHEAD));
        if oracle::Regex::new(&pattern).is_err() || Regex::new(&pattern).is_err() {
            continue;
        }
        compared += 1;
        planned += usize::from(Regex::new(&pattern).unwrap().planned());
        for _ in 0..6 {
            let text: String = (0..rng.below(12)).map(|_| rng.pick(PIECES)).collect();
            compare(&pattern, &text);
        }
    }
    eprintln!("assertions: compared {compared}, planned {planned}");
    assert!(
        compared > 1000 && planned * 3 > compared,
        "{planned} of {compared}"
    );
}
