// Copyright 2026 Notes Supply AB. Distributed under the MIT license.
use crate::{Assertion, Expr, LookAround};
use regex_syntax::hir::{Class, ClassUnicode, ClassUnicodeRange, HirKind};
use std::sync::Arc;

#[derive(Clone)]
pub(crate) enum Atom {
    Class {
        ascii: [u64; 2],
        non_ascii: bool,
        unicode_leads: u64,
        ranges: Arc<[ClassUnicodeRange]>,
    },
    Any {
        newline: bool,
        crlf: bool,
    },
}

#[derive(Clone)]
pub(crate) enum Node {
    Atom(Atom),
    Save(usize),
    Backref {
        group: usize,
        casei: bool,
    },
    Repeat {
        atom: Atom,
        lo: usize,
        hi: usize,
        greedy: bool,
    },
    RepeatGroup {
        body: Vec<Node>,
        width: usize,
        lo: usize,
        hi: usize,
        greedy: bool,
    },
    Alt(Vec<Vec<Node>>),
    Look {
        body: Vec<Node>,
        behind: usize,
        negated: bool,
    },
    Atomic(Vec<Node>),
    Conditional {
        group: usize,
        yes: Vec<Node>,
        no: Vec<Node>,
    },
    Start,
    End,
    Boundary {
        negated: bool,
    },
}

pub(crate) struct Pattern {
    pub(crate) nodes: Vec<Node>,
    pub(crate) groups: usize,
    pub(crate) prefix: Option<Vec<u8>>,
    pub(crate) rich: bool,
}

impl Pattern {
    pub(crate) fn from_expr(expr: &Expr) -> Option<Self> {
        let mut parser = Parser {
            nodes: Vec::new(),
            groups: 0,
            open: Vec::new(),
            count: 0,
        };
        parser.expr(expr)?;
        if node_count(&parser.nodes) > 96 {
            return None;
        }
        // Union every branch's obligatory first byte. A nullable or unknown
        // branch disables the filter; assertions are never consuming prefixes.
        let prefix =
            first_bytes(&parser.nodes).filter(|bytes| !bytes.is_empty() && bytes.len() <= 6);
        let rich = is_rich(expr);
        Some(Self {
            nodes: parser.nodes,
            groups: parser.groups,
            prefix,
            rich,
        })
    }
}

fn mask_contains(mask: [u64; 2], byte: u8) -> bool {
    mask[usize::from(byte / 64)] & (1 << (byte % 64)) != 0
}

fn node_count(nodes: &[Node]) -> usize {
    nodes
        .iter()
        .map(|node| {
            1 + match node {
                Node::RepeatGroup { body, .. } | Node::Look { body, .. } | Node::Atomic(body) => {
                    node_count(body)
                }
                Node::Alt(branches) => branches.iter().map(|branch| node_count(branch)).sum(),
                Node::Conditional { yes, no, .. } => node_count(yes) + node_count(no),
                _ => 0,
            }
        })
        .sum()
}

struct Parser {
    nodes: Vec<Node>,
    groups: usize,
    open: Vec<usize>,
    count: usize,
}

impl Parser {
    fn expr(&mut self, expr: &Expr) -> Option<()> {
        self.count += 1;
        if self.count > 96 {
            return None;
        }
        match expr {
            Expr::Empty => {}
            Expr::Concat(children) => {
                for child in children {
                    self.expr(child)?;
                }
            }
            Expr::Group(child) => {
                self.groups += 1;
                if self.groups > 16 {
                    return None;
                }
                let group = self.groups;
                self.open.push(group);
                self.nodes.push(Node::Save((group - 1) * 2));
                self.expr(child)?;
                self.nodes.push(Node::Save((group - 1) * 2 + 1));
                self.open.pop();
            }
            Expr::Literal { val, casei } => {
                for character in val.chars() {
                    self.nodes.push(Node::Atom(literal(character, *casei)?));
                }
            }
            Expr::Repeat {
                child,
                lo,
                hi,
                greedy,
            } => {
                if let Some(atom) = atom(child) {
                    self.nodes.push(Node::Repeat {
                        atom,
                        lo: *lo,
                        hi: *hi,
                        greedy: *greedy,
                    });
                } else {
                    let body = self.nested(child)?;
                    if *lo == 0 && *hi == 1 {
                        let branches = if *greedy {
                            vec![body, Vec::new()]
                        } else {
                            vec![Vec::new(), body]
                        };
                        self.nodes.push(Node::Alt(branches));
                        return Some(());
                    }
                    // Deterministic, nonempty fixed-character bodies permit
                    // bounded retries without a per-iteration capture stack.
                    let width = deterministic_width(&body)?;
                    if width == 0 || width > 32 {
                        return None;
                    }
                    self.nodes.push(Node::RepeatGroup {
                        body,
                        width,
                        lo: *lo,
                        hi: *hi,
                        greedy: *greedy,
                    });
                }
            }
            Expr::Alt(branches) => {
                let branches = branches
                    .iter()
                    .map(|child| self.nested(child))
                    .collect::<Option<_>>()?;
                self.nodes.push(Node::Alt(branches));
            }
            Expr::LookAround(child, kind) => {
                let body = self.nested(child)?;
                let behind = match kind {
                    LookAround::LookBehind | LookAround::LookBehindNeg => fixed_width(&body)?,
                    _ => 0,
                };
                if behind > 32 {
                    return None;
                }
                self.nodes.push(Node::Look {
                    body,
                    behind,
                    negated: matches!(kind, LookAround::LookAheadNeg | LookAround::LookBehindNeg),
                });
            }
            Expr::AtomicGroup(child) => {
                let body = self.nested(child)?;
                self.nodes.push(Node::Atomic(body));
            }
            Expr::Conditional {
                condition,
                true_branch,
                false_branch,
            } => {
                let Expr::BackrefExistsCondition {
                    group,
                    relative_recursion_level: None,
                } = condition.as_ref()
                else {
                    return None;
                };
                if *group == 0 || *group > self.groups || self.open.contains(group) {
                    return None;
                }
                let yes = self.nested(true_branch)?;
                let no = self.nested(false_branch)?;
                self.nodes.push(Node::Conditional {
                    group: *group,
                    yes,
                    no,
                });
            }
            Expr::Backref { group, casei }
                if *group > 0 && *group <= self.groups && !self.open.contains(group) =>
            {
                self.nodes.push(Node::Backref {
                    group: *group,
                    casei: *casei,
                });
            }
            Expr::Assertion(Assertion::StartText) => self.nodes.push(Node::Start),
            Expr::Assertion(Assertion::EndText) => self.nodes.push(Node::End),
            Expr::Assertion(Assertion::WordBoundary) => {
                self.nodes.push(Node::Boundary { negated: false });
            }
            Expr::Assertion(Assertion::NotWordBoundary) => {
                self.nodes.push(Node::Boundary { negated: true });
            }
            other => self.nodes.push(Node::Atom(atom(other)?)),
        }
        Some(())
    }

    fn nested(&mut self, expr: &Expr) -> Option<Vec<Node>> {
        let outer = std::mem::take(&mut self.nodes);
        let result = self.expr(expr);
        let nested = std::mem::replace(&mut self.nodes, outer);
        result.map(|()| nested)
    }
}

fn is_rich(expr: &Expr) -> bool {
    matches!(
        expr,
        Expr::Backref { .. }
            | Expr::LookAround(..)
            | Expr::AtomicGroup(_)
            | Expr::Conditional { .. }
            | Expr::Assertion(Assertion::WordBoundary | Assertion::NotWordBoundary)
    ) || expr.children_iter().any(is_rich)
}

fn deterministic_width(nodes: &[Node]) -> Option<usize> {
    nodes.iter().try_fold(0usize, |width, node| match node {
        Node::Atom(_) => width.checked_add(1),
        Node::Save(_) => Some(width),
        _ => None,
    })
}

fn fixed_width(nodes: &[Node]) -> Option<usize> {
    nodes.iter().try_fold(0usize, |width, node| {
        let part = match node {
            Node::Atom(_) => 1,
            Node::Save(_) | Node::Start | Node::End | Node::Boundary { .. } | Node::Look { .. } => {
                0
            }
            Node::Repeat { lo, hi, .. } if lo == hi => *lo,
            Node::RepeatGroup { width, lo, hi, .. } if lo == hi => width.checked_mul(*lo)?,
            Node::Alt(branches) => {
                let first = fixed_width(branches.first()?)?;
                if !branches
                    .iter()
                    .all(|branch| fixed_width(branch) == Some(first))
                {
                    return None;
                }
                first
            }
            Node::Atomic(body) => fixed_width(body)?,
            _ => return None,
        };
        width.checked_add(part)
    })
}

fn first_bytes(nodes: &[Node]) -> Option<Vec<u8>> {
    for node in nodes {
        let mut bytes = match node {
            Node::Save(_) | Node::Start | Node::End | Node::Boundary { .. } | Node::Look { .. } => {
                continue;
            }
            Node::Atom(atom) | Node::Repeat { atom, lo: 1.., .. } => atom_bytes(atom)?,
            Node::RepeatGroup { body, lo: 1.., .. } | Node::Atomic(body) => first_bytes(body)?,
            Node::Alt(branches) => {
                let mut bytes = Vec::new();
                for branch in branches {
                    bytes.extend(first_bytes(branch)?);
                }
                bytes
            }
            Node::Conditional { yes, no, .. } => {
                let mut bytes = first_bytes(yes)?;
                bytes.extend(first_bytes(no)?);
                bytes
            }
            _ => return None,
        };
        bytes.sort_unstable();
        bytes.dedup();
        return Some(bytes);
    }
    None
}

fn atom_bytes(atom: &Atom) -> Option<Vec<u8>> {
    let Atom::Class {
        ascii,
        unicode_leads,
        ..
    } = atom
    else {
        return None;
    };
    Some(
        (0u8..128)
            .filter(|&byte| mask_contains(*ascii, byte))
            // These lead bytes cannot occur inside UTF-8 continuation bytes.
            .chain((0xc2u8..=0xf4).filter(|&byte| unicode_leads & (1 << (byte - 0xc0)) != 0))
            .collect(),
    )
}

fn literal(character: char, casei: bool) -> Option<Atom> {
    let mut class = ClassUnicode::new([ClassUnicodeRange::new(character, character)]);
    if casei {
        class.try_case_fold_simple().ok()?;
    }
    Some(unicode_class(&class))
}

fn unicode_class(class: &ClassUnicode) -> Atom {
    let mut ascii = [0u64; 2];
    let mut non_ascii = false;
    let mut unicode_leads = 0;
    for range in class.ranges() {
        non_ascii |= u32::from(range.end()) >= 128;
        if range.end() >= '\u{80}' {
            let first = range
                .start()
                .max('\u{80}')
                .encode_utf8(&mut [0; 4])
                .as_bytes()[0];
            let last = range.end().encode_utf8(&mut [0; 4]).as_bytes()[0];
            for byte in first..=last {
                unicode_leads |= 1 << (byte - 0xc0);
            }
        }
        for value in u32::from(range.start())..=u32::from(range.end()).min(127) {
            ascii[(value / 64) as usize] |= 1 << (value % 64);
        }
    }
    Atom::Class {
        ascii,
        non_ascii,
        unicode_leads,
        ranges: Arc::from(class.ranges()),
    }
}

fn atom(expr: &Expr) -> Option<Atom> {
    match expr {
        Expr::Any { newline, crlf } => Some(Atom::Any {
            newline: *newline,
            crlf: *crlf,
        }),
        Expr::Literal { val, casei } if val.chars().count() == 1 => {
            literal(val.chars().next()?, *casei)
        }
        Expr::Delegate { inner, casei } => {
            let hir = regex_syntax::ParserBuilder::new()
                .case_insensitive(*casei)
                .build()
                .parse(inner)
                .ok()?;
            match hir.kind() {
                HirKind::Class(Class::Unicode(class)) => Some(unicode_class(class)),
                HirKind::Class(Class::Bytes(class)) => {
                    let mut ascii = [0u64; 2];
                    for range in class.ranges() {
                        if range.end() >= 128 {
                            return None;
                        }
                        for byte in range.start()..=range.end() {
                            ascii[usize::from(byte / 64)] |= 1 << (byte % 64);
                        }
                    }
                    Some(Atom::Class {
                        ascii,
                        non_ascii: false,
                        unicode_leads: 0,
                        ranges: Arc::from([]),
                    })
                }
                HirKind::Literal(lit) if lit.0.len() == 1 && lit.0[0].is_ascii() => {
                    literal(char::from(lit.0[0]), false)
                }
                _ => None,
            }
        }
        _ => None,
    }
}
