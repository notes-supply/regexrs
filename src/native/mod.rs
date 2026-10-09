// Copyright 2026 Notes Supply AB. Distributed under the MIT license.
//! Optional direct-code accelerator. Unknown trees and bounded-work deferrals
//! retain the complete portable matcher; they are never false nonmatches.
mod pattern;
mod x64;
pub(crate) use pattern::Pattern;

/// Compilation failed without changing any running matcher.
#[derive(Debug, thiserror::Error)]
#[error("native regex compilation failed: {0}")]
pub(crate) struct CompileError(String);

impl CompileError {
    fn from_display(error: impl std::fmt::Display) -> Self {
        Self(error.to_string())
    }
}

type MatchFn = unsafe extern "C" fn(*const u8, usize, usize, *mut usize) -> u32;

/// Owns executable memory and a matcher with no per-opcode runtime dispatch.
///
/// Not a replacement for the whole regexrs interface. The only supported
/// interface here is boolean matching over valid UTF-8, with explicit fallback.
pub(crate) struct NativeRegex {
    // Keep the executable allocation alive for every call through `matcher`.
    _module: CodeOwner,
    matcher: MatchFn,
    prefix: Option<Vec<u8>>,
    code_bytes: usize,
}

enum CodeOwner {
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    Direct {
        _buffer: dynasmrt::ExecutableBuffer,
        // Machine code borrows these immutable, address-stable range tables.
        _classes: Vec<std::sync::Arc<[regex_syntax::hir::ClassUnicodeRange]>>,
    },
}

type Emit = Box<dyn FnOnce() -> Option<NativeRegex> + Send + std::panic::UnwindSafe>;

/// Native code for a validated, lowered pattern, emitted on first use.
/// Expressions never searched for a boolean answer over a whole UTF-8 string
/// pay neither code generation nor an executable mapping.
pub(crate) struct LazyNative(std::sync::LazyLock<Option<NativeRegex>, Emit>);

impl LazyNative {
    pub(crate) fn new(pattern: pattern::Pattern) -> Self {
        Self(std::sync::LazyLock::new(Box::new(move || {
            x64::compile(pattern).ok().flatten()
        })))
    }

    /// The emitted matcher, or `None` when emission was not possible.
    pub(crate) fn get(&self) -> Option<&NativeRegex> {
        std::sync::LazyLock::force(&self.0).as_ref()
    }
}

impl NativeRegex {
    /// Match, or defer the entire input to the reference engine.
    pub fn is_match(&self, text: &str) -> Option<bool> {
        // Shared by every attempted start and every generated repeat/backref.
        // A cap bounds the accelerator's added work, not the reference's errors.
        let mut fuel = 100_000;
        if let Some(prefix) = self.prefix.as_deref() {
            let mut start = 0;
            while start < text.len() {
                let bytes = &text.as_bytes()[start..];
                let relative = prefix
                    .chunks(3)
                    .filter_map(|chunk| match chunk {
                        [a] => memchr::memchr(*a, bytes),
                        [a, b] => memchr::memchr2(*a, *b, bytes),
                        [a, b, c] => memchr::memchr3(*a, *b, *c, bytes),
                        _ => None,
                    })
                    .min();
                let Some(relative) = relative else {
                    return Some(false);
                };
                start += relative;
                match self.at(text, start, &mut fuel)? {
                    true => return Some(true),
                    false => start += 1,
                }
            }
            Some(false)
        } else {
            for (start, _) in text.char_indices() {
                if self.at(text, start, &mut fuel)? {
                    return Some(true);
                }
            }
            self.at(text, text.len(), &mut fuel)
        }
    }

    fn at(&self, text: &str, start: usize, fuel: &mut usize) -> Option<bool> {
        *fuel = fuel.checked_sub(1)?;
        // SAFETY: start is a UTF-8 boundary, the pointer is readable for len
        // bytes for the duration of the call, generated code only reads it, and
        // `_module` has not been dropped. `fuel` is exclusively borrowed and
        // its live counter is not aliased by the input. The private helpers
        // cannot unwind for valid inputs; no panic crosses generated code.
        match unsafe { (self.matcher)(text.as_ptr(), text.len(), start, fuel) } {
            0 => Some(false),
            1 => Some(true),
            _ => None,
        }
    }

    /// Size of this pattern's native instruction buffer, excluding metadata.
    pub fn code_bytes(&self) -> usize {
        self.code_bytes
    }
}

extern "C" fn word_boundary(text: *const u8, len: usize, position: usize) -> u32 {
    // SAFETY: generated code receives a live immutable UTF-8 string from `at`;
    // its consuming instructions/retries maintain character boundaries.
    let text = unsafe { std::str::from_utf8_unchecked(std::slice::from_raw_parts(text, len)) };
    let (Some(left), Some(right)) = (text.get(..position), text.get(position..)) else {
        return 2;
    };
    let word = |character| match character {
        Some(character) => regex_syntax::try_is_word_character(character).ok(),
        None => Some(false),
    };
    match (word(left.chars().next_back()), word(right.chars().next())) {
        (Some(left), Some(right)) => u32::from(left != right),
        _ => 2,
    }
}

extern "C" fn unicode_class(
    text: *const u8,
    len: usize,
    position: usize,
    ranges: *const regex_syntax::hir::ClassUnicodeRange,
    count: usize,
) -> usize {
    // SAFETY: the generated caller supplies its live UTF-8 input and an
    // immutable range allocation retained by the executable's CodeOwner.
    let text = unsafe { std::str::from_utf8_unchecked(std::slice::from_raw_parts(text, len)) };
    let ranges = unsafe { std::slice::from_raw_parts(ranges, count) };
    let Some(character) = text.get(position..).and_then(|tail| tail.chars().next()) else {
        return usize::MAX;
    };
    let index = ranges.partition_point(|range| range.end() < character);
    if ranges
        .get(index)
        .is_some_and(|range| range.start() <= character)
    {
        position + character.len_utf8()
    } else {
        usize::MAX
    }
}

extern "C" fn unicode_backref(
    text: *const u8,
    len: usize,
    position: usize,
    capture: usize,
    width: usize,
) -> u32 {
    // SAFETY: NativeRegex::at owns this live immutable input. The caller checks
    // both capture and target bounds; get additionally checks UTF-8 boundaries.
    let text = unsafe { std::str::from_utf8_unchecked(std::slice::from_raw_parts(text, len)) };
    let (Some(left), Some(right)) = (
        text.get(capture..capture + width),
        text.get(position..position + width),
    ) else {
        return 0;
    };
    // Match the oracle's captured BYTE length, not a different-width Unicode
    // spelling. Most Unicode captures compare equal in the emitted byte loop.
    let mut left = left.chars();
    let mut right = right.chars();
    loop {
        match (left.next(), right.next()) {
            (None, None) => return 1,
            (Some(a), Some(b)) if a == b => {}
            (Some(a), Some(b)) if a.is_ascii() && b.is_ascii() => {
                if !a.eq_ignore_ascii_case(&b) {
                    return 0;
                }
            }
            (Some(a), Some(b)) => {
                let mut class = regex_syntax::hir::ClassUnicode::new([
                    regex_syntax::hir::ClassUnicodeRange::new(a, a),
                ]);
                if class.try_case_fold_simple().is_err()
                    || !class
                        .ranges()
                        .iter()
                        .any(|range| range.start() <= b && b <= range.end())
                {
                    return 0;
                }
            }
            _ => return 0,
        }
    }
}

extern "C" fn scan_any(text: *const u8, len: usize, start: usize, crlf: usize) -> usize {
    if start > len {
        return len;
    }
    // SAFETY: this private symbol is called only by generated code owned by
    // NativeRegex::at, using that call's live, immutable input pointer/length.
    let bytes = unsafe { std::slice::from_raw_parts(text, len) };
    let tail = &bytes[start..];
    let newline = if crlf == 0 {
        memchr::memchr(b'\n', tail)
    } else {
        memchr::memchr2(b'\n', b'\r', tail)
    };
    start + newline.unwrap_or(tail.len())
}

#[cfg(test)]
mod tests {
    use crate::Regex;

    #[test]
    fn native_decisions_match_oracle_without_hidden_portable_fallback() {
        let patterns = [
            r"(a|ab)\1",
            r"(?i:(é)\1)",
            r"(?i:(Σ)\1)",
            r"(?<=a)b",
            r"a(?!b)",
            r"(?>a|ab)c",
            r"(a)?(?(1)b|c)",
        ];
        let mut texts = alloc::vec![alloc::string::String::new()];
        let mut previous = texts.clone();
        for _ in 0..3 {
            let mut next = alloc::vec::Vec::new();
            for prefix in previous {
                for suffix in ["a", "b", "c", "é", "É", "Σ", "σ", "ς", " "] {
                    next.push(alloc::format!("{prefix}{suffix}"));
                }
            }
            texts.extend(next.iter().cloned());
            previous = next;
        }
        for pattern in patterns {
            let regex = Regex::new(pattern).unwrap();
            let native = regex
                .native
                .as_ref()
                .and_then(|native| native.get())
                .expect("pattern should be accelerated");
            let oracle = oracle::Regex::new(pattern).unwrap();
            for text in &texts {
                assert_eq!(
                    native.is_match(text),
                    Some(oracle.is_match(text).unwrap()),
                    "{pattern} {text:?}"
                );
            }
        }
    }

    #[test]
    fn exhausted_native_work_returns_deferral_not_a_false_nonmatch() {
        let regex = Regex::new(r"\A(a+)\1\z").unwrap();
        let native = regex
            .native
            .as_ref()
            .and_then(|native| native.get())
            .unwrap();
        let input = "a".repeat(100_002);
        assert_eq!(native.is_match(&input), None);
        assert!(regex.is_match(&input).unwrap());
    }
}
