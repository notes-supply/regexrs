// Copyright 2026 Notes Supply AB. Distributed under the MIT license.
//! Synthetic, parity-gated comparisons against published Rust engines.
//! `cargo run --release --example competition --features native -- bench 1 1000000 7`
//! Use `cases` for pattern indices and `conformance` for targeted oracle checks.
//! Input generation, compilation and output are excluded from scan timings.
use regexrs::RegexBuilder;
use serde_json::{Value, json};
use std::{hint::black_box, time::Instant};

const ENGINES: [&str; 8] = [
    "regexrs",
    "regexrs-portable",
    "regexrs-cached",
    "fancy",
    "fancy-seek",
    "regexr-jit",
    "regexr",
    "regress",
];
const CASES: [(&str, &str); 17] = [
    ("ordinary", r"key-731 "),
    ("native_backref", r"#(\d+) .*event \1:"),
    ("word_boundary", r"\bkey-731\b"),
    ("lookbehind", r"(?<=key-)731\b"),
    ("positive_lookahead", r"key-(?=731\b)\d+"),
    ("negative_lookahead", r"key-(?!731\b)\d+"),
    ("alternation_backref", r"#(\d+) .*?(?:event|incident) \1:"),
    ("repeat_group_backref", r"((?:synthetic )+)(?:event).*?\1"),
    (
        "sparse_alternation_backref",
        r"(?:absent|missing)(\d+): key-\1",
    ),
    ("unicode_backref", r"(café).*?\1"),
    ("lookahead_backref", r"(?P<word>event)(?= ) (?P=word)"),
    ("conditional", r"event (\d+): (?(1)key-\1|none)"),
    ("unicode_casefold_hit", r"(café) \1"),
    ("group_repeat_hit", r"((?:synthetic )+).*?\1"),
    ("named_backref_hit", r"(?P<word>event)(?= ) (?P=word)"),
    ("unsupported_native_repeat", r"((a|ab)+)\1"),
    ("atomic", r"(?>event|incident) \d+:"),
];

enum Engine {
    Regexrs(regexrs::Regex),
    RegexrsCached(regexrs::Regex),
    Fancy(oracle::Regex),
    Regexr(Box<regexr::Regex>),
    Regress(regress::Regex),
    Regular(regex::Regex),
}

impl Engine {
    fn compile(name: &str, pattern: &str) -> Result<Self, String> {
        match name {
            "regexrs" | "regexrs-portable" | "regexrs-cached" => {
                let regex = RegexBuilder::new(pattern)
                    .case_insensitive(true)
                    .native(name == "regexrs")
                    .build()
                    .map_err(|e| e.to_string())?;
                Ok(if name == "regexrs-cached" {
                    Self::RegexrsCached(regex)
                } else {
                    Self::Regexrs(regex)
                })
            }
            "fancy" | "fancy-seek" => oracle::RegexBuilder::new(pattern)
                .case_insensitive(true)
                .seek(name == "fancy-seek")
                .build()
                .map(Self::Fancy)
                .map_err(|e| e.to_string()),
            "regexr" | "regexr-jit" => regexr::RegexBuilder::new(&format!("(?i:{pattern})"))
                .jit(name == "regexr-jit")
                .build()
                .map(|regex| Self::Regexr(Box::new(regex)))
                .map_err(|e| e.to_string()),
            "regress" => {
                // Syntax-only translation of this suite's single named capture/backref.
                let pattern = pattern
                    .replace("(?P<word>", "(?<word>")
                    .replace("(?P=word)", r"\k<word>");
                regress::Regex::with_flags(
                    &pattern,
                    regress::Flags {
                        icase: true,
                        unicode: true,
                        ..Default::default()
                    },
                )
                .map(Self::Regress)
                .map_err(|e| e.to_string())
            }
            "regex" => regex::RegexBuilder::new(pattern)
                .case_insensitive(true)
                .build()
                .map(Self::Regular)
                .map_err(|e| e.to_string()),
            _ => Err(format!("unknown engine {name}")),
        }
    }

    fn is_match(&self, text: &str) -> Result<bool, String> {
        match self {
            Self::RegexrsCached(r) => r.searcher().is_match(text).map_err(|e| e.to_string()),
            Self::Regexrs(r) => r.is_match(text).map_err(|e| e.to_string()),
            Self::Fancy(r) => r.is_match(text).map_err(|e| e.to_string()),
            Self::Regexr(r) => r.try_is_match(text).map_err(|e| e.to_string()),
            Self::Regress(r) => Ok(r.find(text).is_some()),
            Self::Regular(r) => Ok(r.is_match(text)),
        }
    }

    fn scan(&self, texts: &[String]) -> Result<usize, String> {
        // Select once per scan, not a virtual/enum dispatch per record.
        fn run(
            texts: &[String],
            mut matches: impl FnMut(&str) -> Result<bool, String>,
        ) -> Result<usize, String> {
            texts.iter().try_fold(0, |hits, text| {
                Ok(hits + usize::from(matches(black_box(text))?))
            })
        }
        match self {
            Self::RegexrsCached(r) => {
                let mut search = r.searcher();
                run(texts, |t| search.is_match(t).map_err(|e| e.to_string()))
            }
            Self::Regexrs(r) => run(texts, |t| r.is_match(t).map_err(|e| e.to_string())),
            Self::Fancy(r) => run(texts, |t| r.is_match(t).map_err(|e| e.to_string())),
            Self::Regexr(r) => run(texts, |t| r.try_is_match(t).map_err(|e| e.to_string())),
            Self::Regress(r) => run(texts, |t| Ok(r.find(t).is_some())),
            Self::Regular(r) => run(texts, |t| Ok(r.is_match(t))),
        }
    }

    fn route(&self) -> String {
        match self {
            Self::Regexrs(r) | Self::RegexrsCached(r) => if r.has_native_code() {
                "native candidate"
            } else {
                "reference route"
            }
            .into(),
            Self::Regexr(r) => r.engine_name().into(),
            Self::Fancy(_) => "published fancy-regex".into(),
            Self::Regress(_) => "regress default optimized backend".into(),
            Self::Regular(_) => "regular regex".into(),
        }
    }
}

fn fixture(case: &str, count: usize) -> Vec<String> {
    (0..count)
        .map(|i| {
            let tail = match case {
                "unicode_casefold_hit" => {
                    if i % 2 == 0 {
                        " café"
                    } else {
                        " CAFÉ"
                    }
                }
                "group_repeat_hit" => {
                    if i % 2 == 0 {
                        " synthetic "
                    } else {
                        " unrelated "
                    }
                }
                "named_backref_hit" => {
                    if i % 2 == 0 {
                        " event event"
                    } else {
                        " event incident"
                    }
                }
                "unsupported_native_repeat" => {
                    if i % 2 == 0 {
                        " abab"
                    } else {
                        " ac"
                    }
                }
                _ => "",
            };
            format!(
                "#{i} 2026-01-01 synthetic event {i}: key-{} café{tail}",
                i % 1000
            )
        })
        .collect()
}

fn bench(case_index: usize, count: usize, samples: usize) -> Result<Value, String> {
    let (case, pattern) = CASES[case_index];
    let texts = fixture(case, count);
    let oracle = Engine::compile("fancy", pattern)?;
    let expected: Vec<bool> = texts
        .iter()
        .map(|t| oracle.is_match(t))
        .collect::<Result<_, _>>()?;
    let hits = expected.iter().filter(|&&x| x).count();
    let mut names = ENGINES.to_vec();
    if matches!(case, "ordinary" | "word_boundary") {
        names.push("regex");
    }
    let mut compiled = Vec::new();
    let mut report = serde_json::Map::new();
    let len = names.len();
    names.rotate_left(case_index % len);
    for name in &names {
        let start = Instant::now();
        let regex = match Engine::compile(name, pattern) {
            Ok(regex) => regex,
            Err(error) => {
                report.insert(
                    (*name).into(),
                    json!({"status":"compile_error","error":error}),
                );
                continue;
            }
        };
        let first_build_us = start.elapsed().as_secs_f64() * 1e6;
        let mut mismatches = 0;
        let mut first_error = None;
        for (text, &expected) in texts.iter().zip(&expected) {
            match regex.is_match(text) {
                Ok(found) => mismatches += usize::from(found != expected),
                Err(error) => {
                    first_error = Some(error);
                    break;
                }
            }
        }
        if mismatches != 0 || first_error.is_some() {
            report.insert((*name).into(), json!({"status":"excluded","mismatched_records":mismatches,"match_error":first_error,"route":regex.route()}));
            continue;
        }
        regex.scan(&texts)?; // Warm each valid engine once.
        report.insert((*name).into(), json!({"status":"parity_pass","route":regex.route(),"first_build_us":first_build_us,"compile_us":[],"scan_ms":[]}));
        compiled.push(((*name).to_owned(), regex));
    }
    // Rotation pairs scans under the same process, fixture, and CPU.
    for sample in 0..samples {
        for k in 0..compiled.len() {
            let (name, regex) = &compiled[(sample + k) % compiled.len()];
            let start = Instant::now();
            let actual = black_box(regex.scan(black_box(&texts))?);
            let elapsed = start.elapsed().as_secs_f64() * 1e3;
            if actual != hits {
                return Err(format!("{name}: hit count changed"));
            }
            report[name]["scan_ms"]
                .as_array_mut()
                .unwrap()
                .push(json!(elapsed));
        }
    }
    for sample in 0..31 {
        for k in 0..compiled.len() {
            let (name, _) = &compiled[(sample + k) % compiled.len()];
            let start = Instant::now();
            let regex = black_box(Engine::compile(name, black_box(pattern))?);
            let elapsed = start.elapsed().as_secs_f64() * 1e6;
            report[name]["compile_us"]
                .as_array_mut()
                .unwrap()
                .push(json!(elapsed));
            drop(regex);
        }
    }
    Ok(
        json!({"case":case,"pattern":pattern,"records":count,"hits":hits,"samples":samples,"engines":report}),
    )
}

fn conformance() -> Result<Value, String> {
    let patterns = [
        r"(a|ab)\1",
        r"((a|ab)+)\1",
        r"(a*)*\1",
        r"(a)?(?(1)\1|b)",
        r"(?>a|ab)c",
        r"(a|ab)(?=c)\1",
        r"(?<=(a|b))\1",
        r"(?<!é)é",
        r"(k)\1",
        r"(K)\1",
        r"(s)\1",
        r"(ſ)\1",
        r"\bélan\b",
        r"(é)\1",
        r"([α-ω])\1",
        r"(?P<word>a)(?P=word)",
        r"(a)?b\1",
    ];
    let mut texts = vec![String::new()];
    let mut level = vec![String::new()];
    for _ in 0..4 {
        level = level
            .iter()
            .flat_map(|s| ['a', 'b', 'c', 'é'].map(|c| format!("{s}{c}")))
            .collect();
        texts.extend(level.iter().cloned());
    }
    texts.extend(
        [
            "kk", "kK", "Kk", "KK", "ss", "sſ", "ſs", "ſſ", "élan", " élan ", "éÉ", "αΑ", "a\na",
        ]
        .map(str::to_owned),
    );
    let mut report = Vec::new();
    for pattern in patterns {
        let oracle = Engine::compile("fancy", pattern)?;
        let expected: Vec<_> = texts
            .iter()
            .map(|t| oracle.is_match(t))
            .collect::<Result<_, _>>()?;
        let mut engines = serde_json::Map::new();
        for name in ENGINES {
            match Engine::compile(name, pattern) {
                Err(error) => {
                    engines.insert(name.into(), json!({"status":"compile_error","error":error}));
                }
                Ok(regex) => {
                    let mut differences = Vec::new();
                    for (text, &expected) in texts.iter().zip(&expected) {
                        match regex.is_match(text) {
                            Ok(found) if found == expected => (),
                            result => differences.push(json!({"text":text,"expected":expected,"result":format!("{result:?}")})),
                        }
                    }
                    engines.insert(
                        name.into(),
                        json!({"differences":differences,"route":regex.route()}),
                    );
                }
            }
        }
        report.push(json!({"pattern":pattern,"inputs":texts.len(),"engines":engines}));
    }
    Ok(json!(report))
}

fn main() {
    let args: Vec<_> = std::env::args().collect();
    let result = match args.get(1).map(String::as_str) {
        Some("conformance") => conformance(),
        Some("cases") => Ok(json!(CASES)),
        Some("compile") => {
            let index: usize = args[3].parse().unwrap();
            let start = Instant::now();
            Engine::compile(&args[2], CASES[index].1).map(|regex| {
                let elapsed = start.elapsed().as_secs_f64() * 1e6;
                json!({"compile_us":elapsed,"route":black_box(regex).route()})
            })
        }
        Some("bench") => bench(
            args[2].parse().unwrap(),
            args[3].parse().unwrap(),
            args[4].parse().unwrap(),
        ),
        _ => Err("usage: competition conformance|cases|bench INDEX RECORDS SAMPLES".into()),
    };
    match result {
        Ok(report) => println!("{}", report),
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(1);
        }
    }
}
