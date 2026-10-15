use alloc::borrow::Cow;
use alloc::boxed::Box;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use crate::parse::{ParsedId, parse_id, parse_usize};
use crate::{Captures, CompileError, Error, ParseError, Regex};

/// A set of options for expanding a template string using the contents
/// of capture groups.
#[derive(Debug)]
pub struct Expander {
    sub_char: char,
    open: &'static str,
    close: &'static str,
    allow_undelimited_name: bool,
}

impl Default for Expander {
    /// Returns the default expander used by [`Captures::expand`].
    ///
    /// [`Captures::expand`]: struct.Captures.html#expand
    fn default() -> Self {
        Expander {
            sub_char: '$',
            open: "{",
            close: "}",
            allow_undelimited_name: true,
        }
    }
}

impl Expander {
    /// Returns an expander that uses Python-compatible syntax.
    ///
    /// Expands all instances of `\num` or `\g<name>` in `replacement`
    /// to the corresponding capture group `num` or `name`, and writes
    /// them to the `dst` buffer given.
    ///
    /// `name` may be an integer corresponding to the index of the
    /// capture group (counted by order of opening parenthesis where `\0` is the
    /// entire match) or it can be a name (consisting of letters, digits or
    /// underscores) corresponding to a named capture group.
    ///
    /// `num` must be an integer corresponding to the index of the
    /// capture group.
    ///
    /// If `num` or `name` isn't a valid capture group (whether the name doesn't exist
    /// or isn't a valid index), then it is replaced with the empty string.
    ///
    /// The longest possible number is used. e.g., `\10` looks up capture
    /// group 10 and not capture group 1 followed by a literal 0.
    ///
    /// To write a literal `\`, use `\\`.
    pub fn python() -> Expander {
        Expander {
            sub_char: '\\',
            open: "g<",
            close: ">",
            allow_undelimited_name: false,
        }
    }

    /// Checks `template` for errors.  The following conditions are checked for:
    ///
    /// - A reference to a numbered group that does not exist in `regex`
    /// - A reference to a numbered group (other than 0) when `regex` contains named groups
    /// - A reference to a named group that does not occur in `regex`
    /// - An opening group name delimiter without a closing delimiter
    /// - Using an empty string as a group name
    pub fn check(&self, template: &str, regex: &Regex) -> crate::Result<()> {
        let on_group_num = |num| {
            if num == 0 {
                Ok(())
            } else if !regex.named_groups.is_empty() {
                Err(Error::CompileError(Box::new(
                    CompileError::NamedBackrefOnly,
                )))
            } else if num < regex.captures_len() {
                Ok(())
            } else {
                Err(Error::CompileError(Box::new(CompileError::InvalidBackref(
                    num,
                ))))
            }
        };
        self.exec(template, |step| match step {
            Step::Literal(_) => Ok(()),
            Step::GroupName(name) => {
                if regex.named_groups.contains_key(name) {
                    Ok(())
                } else if let Ok(num) = name.parse() {
                    on_group_num(num)
                } else {
                    Err(Error::CompileError(Box::new(
                        CompileError::InvalidGroupNameBackref(name.to_string()),
                    )))
                }
            }
            Step::GroupNum(num) => on_group_num(num),
            Step::Error => Err(Error::ParseError(
                0,
                ParseError::GeneralParseError(
                    "parse error in template while expanding".to_string(),
                ),
            )),
        })
    }

    /// Escapes the substitution character in `text` so it appears literally
    /// in the output of `expansion`.
    ///
    /// ```
    /// assert_eq!(
    ///     regexrs::Expander::default().escape("Has a literal $ sign."),
    ///     "Has a literal $$ sign.",
    /// );
    /// ```
    pub fn escape<'a>(&self, text: &'a str) -> Cow<'a, str> {
        if text.contains(self.sub_char) {
            let mut quoted = String::with_capacity(self.sub_char.len_utf8() * 2);
            quoted.push(self.sub_char);
            quoted.push(self.sub_char);
            Cow::Owned(text.replace(self.sub_char, &quoted))
        } else {
            Cow::Borrowed(text)
        }
    }

    #[doc(hidden)]
    #[deprecated(since = "0.4.0", note = "Use `escape` instead.")]
    pub fn quote<'a>(&self, text: &'a str) -> Cow<'a, str> {
        self.escape(text)
    }

    /// Expands the template string `template` using the syntax defined
    /// by this expander and the values of capture groups from `captures`.
    pub fn expansion(&self, template: &str, captures: &Captures<'_, str>) -> String {
        let mut expanded = String::with_capacity(template.len());
        self.append_expansion(&mut expanded, template, captures);
        expanded
    }

    /// Appends the expansion produced by `expansion` to `dst`.  Potentially more efficient
    /// than calling `expansion` directly and appending to an existing string.
    pub fn append_expansion(&self, dst: &mut String, template: &str, captures: &Captures<'_, str>) {
        self.expand_into(template, captures, |piece| dst.push_str(piece));
    }

    /// Writes the expansion produced by `expansion` to `dst`.  Potentially more efficient
    /// than calling `expansion` directly and writing the result.
    #[cfg(feature = "std")]
    pub fn write_expansion(
        &self,
        mut dst: impl std::io::Write,
        template: &str,
        captures: &Captures<'_, str>,
    ) -> std::io::Result<()> {
        self.exec(template, |step| match group_text(step, captures) {
            Some(piece) => dst.write_all(piece.as_bytes()),
            None => Ok(()),
        })
    }

    /// Writes the expansion produced by `expansion` to `dst`.  Potentially more efficient
    /// than calling `expansion` directly and writing the result.
    pub fn write_expansion_vec(
        &self,
        dst: &mut Vec<u8>,
        template: &str,
        captures: &Captures<'_, str>,
    ) -> core::fmt::Result {
        self.expand_into(template, captures, |piece| {
            dst.extend_from_slice(piece.as_bytes())
        });
        Ok(())
    }

    /// Pass each piece of the expansion, in order, to `push`.
    fn expand_into<'t>(
        &self,
        template: &'t str,
        captures: &Captures<'t, str>,
        mut push: impl FnMut(&'t str),
    ) {
        let expanded: Result<(), core::convert::Infallible> = self.exec(template, |step| {
            if let Some(piece) = group_text(step, captures) {
                push(piece);
            }
            Ok(())
        });
        if let Err(never) = expanded {
            match never {}
        }
    }

    /// Parse `template` and pass its steps to `f`: runs of literal text whole,
    /// so expanding costs one pass over the template.
    fn exec<'t, E>(
        &self,
        template: &'t str,
        mut f: impl FnMut(Step<'t>) -> Result<(), E>,
    ) -> Result<(), E> {
        debug_assert!(!self.open.is_empty());
        debug_assert!(!self.close.is_empty());
        let sub_len = self.sub_char.len_utf8();
        let mut rest = template;
        while let Some(at) = rest.find(self.sub_char) {
            if at > 0 {
                f(Step::Literal(&rest[..at]))?;
            }
            let sub = &rest[at..at + sub_len];
            let tail = &rest[at + sub_len..];
            let skip = if tail.starts_with(self.sub_char) {
                f(Step::Literal(sub))?;
                sub_len
            } else if let Some(ParsedId {
                id,
                relative: None,
                skip,
            }) = parse_id(tail, self.open, self.close, false).or_else(|| {
                if self.allow_undelimited_name {
                    parse_id(tail, "", "", false)
                } else {
                    None
                }
            }) {
                f(Step::GroupName(id))?;
                skip
            } else if let Some((skip, num)) = parse_usize(tail, 0) {
                f(Step::GroupNum(num))?;
                skip
            } else {
                f(Step::Error)?;
                f(Step::Literal(sub))?;
                0
            };
            rest = &tail[skip..];
        }
        if !rest.is_empty() {
            f(Step::Literal(rest))?;
        }
        Ok(())
    }
}

/// The text `step` adds to an expansion: its literal text, or the group it
/// names, by name first and then by number. `None` adds nothing.
fn group_text<'t>(step: Step<'t>, captures: &Captures<'t, str>) -> Option<&'t str> {
    let group = match step {
        Step::Literal(text) => return Some(text),
        Step::GroupName(name) => captures
            .name(name)
            .or_else(|| name.parse().ok().and_then(|num| captures.get(num))),
        Step::GroupNum(num) => captures.get(num),
        Step::Error => None,
    };
    group.map(|m| m.as_str())
}

enum Step<'a> {
    Literal(&'a str),
    GroupName(&'a str),
    GroupNum(usize),
    Error,
}
