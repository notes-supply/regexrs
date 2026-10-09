// Copyright 2026 Notes Supply AB. Distributed under the MIT license.
//! Direct System V x86-64 templates: no optimizer or register-allocation pass.
//!
//! r12 = borrowed text, r13 = length, r14 = UTF-8 cursor, r15 = work budget.
//! All four are callee-saved. Captures and retry snapshots use a bounded frame.
use super::{
    CodeOwner, CompileError, MatchFn, NativeRegex,
    pattern::{Atom, Node, Pattern},
};
use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, dynasm};
use std::sync::Arc;

type Assembler = dynasmrt::x64::Assembler;

#[derive(Clone, Copy)]
struct Repetition {
    lo: usize,
    hi: usize,
    greedy: bool,
}

pub(crate) fn compile(pattern: Pattern) -> Result<Option<NativeRegex>, CompileError> {
    let mut ops = Assembler::new().map_err(CompileError::from_display)?;
    let no = ops.new_dynamic_label();
    let yes = ops.new_dynamic_label();
    let defer = ops.new_dynamic_label();
    let exit = ops.new_dynamic_label();
    let entry = ops.new_dynamic_label();
    let body = ops.new_dynamic_label();
    let slots = pattern.groups * 2;
    // Emit the body first to determine its bounded frame, including duplicated
    // continuations. These two direct jumps avoid patching executable code.
    dynasm!(ops; .arch x64; jmp =>entry; =>body);
    let mut emitter = Emitter {
        ops,
        slots,
        next_frame: (slots * 8) as i32,
        labels: 6,
        defer,
        unicode_stubs: Vec::new(),
    };
    if emitter.sequence(&pattern.nodes, no, yes).is_none() {
        return Ok(None);
    }
    let classes = emitter.unicode_stubs();
    let required = emitter.next_frame as usize + 8;
    // Entry rsp is 8 mod 16; four saved registers retain that alignment.
    // An 8-mod-16 frame makes Rust helper calls correctly aligned.
    let frame = (required.div_ceil(16) * 16 + 8) as i32;
    let mut ops = emitter.ops;
    dynasm!(ops
        ; =>no; xor eax, eax; jmp =>exit
        ; =>yes; mov eax, 1; jmp =>exit
        ; =>defer; mov eax, 2
        ; =>exit
        ; mov rdx, QWORD [rsp + frame - 8]; mov QWORD [rdx], r15
        ; add rsp, frame
        ; pop r15; pop r14; pop r13; pop r12
        ; ret
        ; =>entry
        ; push r12; push r13; push r14; push r15
        ; mov r12, rdi; mov r13, rsi; mov r14, rdx
        ; mov r15, QWORD [rcx]
    );
    // Touch each allocated page rather than jumping over a stack guard page.
    let mut remaining = frame;
    while remaining >= 4096 {
        dynasm!(ops; sub rsp, 4096; mov QWORD [rsp], 0);
        remaining -= 4096;
    }
    dynasm!(ops; sub rsp, remaining);
    dynasm!(ops; mov QWORD [rsp + frame - 8], rcx);
    for slot in 0..slots {
        dynasm!(ops; mov QWORD [rsp + (slot * 8) as i32], -1);
    }
    dynasm!(ops; jmp =>body);
    let code_bytes = ops.offset().0;
    ops.commit().map_err(CompileError::from_display)?;
    let buffer = ops
        .finalize()
        .map_err(|_| CompileError("native code still has an outstanding owner".into()))?;
    // SAFETY: the templates implement MatchFn's System V ABI. Input byte loads
    // are bounded, all mutable state is local to this call, preserved registers
    // are restored, and NativeRegex owns the executable mapping for every call.
    let matcher = unsafe {
        std::mem::transmute::<*const u8, MatchFn>(buffer.ptr(dynasmrt::AssemblyOffset(0)))
    };
    Ok(Some(NativeRegex {
        _module: CodeOwner::Direct {
            _buffer: buffer,
            _classes: classes,
        },
        matcher,
        prefix: pattern.prefix,
        code_bytes,
    }))
}

struct Emitter {
    ops: Assembler,
    slots: usize,
    next_frame: i32,
    labels: usize,
    defer: DynamicLabel,
    unicode_stubs: Vec<UnicodeStub>,
}

struct UnicodeStub {
    entry: DynamicLabel,
    resume: DynamicLabel,
    failure: DynamicLabel,
    ranges: Arc<[regex_syntax::hir::ClassUnicodeRange]>,
}

impl Emitter {
    fn unicode_stubs(&mut self) -> Vec<Arc<[regex_syntax::hir::ClassUnicodeRange]>> {
        // Keep cold decoding off the ASCII fall-through path; matching does
        // not allocate, lock or dispatch an opcode for a class lookup.
        std::mem::take(&mut self.unicode_stubs).into_iter().map(|stub| {
            dynasm!(self.ops
                ; =>stub.entry
                ; mov rdi, r12; mov rsi, r13; mov rdx, r14
                ; mov rcx, QWORD stub.ranges.as_ptr() as i64; mov r8, QWORD stub.ranges.len() as i64
                ; mov rax, QWORD super::unicode_class as *const () as i64; call rax
                ; cmp rax, -1; je =>stub.failure; mov r14, rax; jmp =>stub.resume
            );
            stub.ranges
        }).collect()
    }

    fn label(&mut self) -> Option<DynamicLabel> {
        self.labels += 1;
        (self.labels <= 4096).then(|| self.ops.new_dynamic_label())
    }

    fn tick(&mut self) {
        let defer = self.defer;
        dynasm!(self.ops; test r15, r15; jz =>defer; dec r15);
    }

    fn reserve(&mut self) -> Option<i32> {
        let base = self.next_frame;
        self.next_frame += (self.slots * 8 + 24) as i32;
        (self.next_frame <= 262_144).then_some(base)
    }

    fn sequence(
        &mut self,
        nodes: &[Node],
        failure: DynamicLabel,
        success: DynamicLabel,
    ) -> Option<()> {
        for (index, node) in nodes.iter().enumerate() {
            match node {
                Node::Atom(atom) => self.atom(atom, failure)?,
                Node::Save(slot) => dynasm!(self.ops; mov QWORD [rsp + (slot * 8) as i32], r14),
                Node::Backref { group, casei } => self.backref(*group, *casei, failure)?,
                Node::Repeat {
                    atom,
                    lo,
                    hi,
                    greedy,
                } => {
                    return self.repeat(
                        atom,
                        Repetition {
                            lo: *lo,
                            hi: *hi,
                            greedy: *greedy,
                        },
                        &nodes[index + 1..],
                        failure,
                        success,
                    );
                }
                Node::RepeatGroup {
                    body,
                    width,
                    lo,
                    hi,
                    greedy,
                } => {
                    return self.repeat_group(
                        body,
                        *width,
                        Repetition {
                            lo: *lo,
                            hi: *hi,
                            greedy: *greedy,
                        },
                        &nodes[index + 1..],
                        failure,
                        success,
                    );
                }
                Node::Alt(branches) => {
                    return self.alternation(branches, &nodes[index + 1..], failure, success);
                }
                Node::Look {
                    body,
                    behind,
                    negated,
                } => self.look(body, *behind, *negated, failure)?,
                Node::Atomic(body) => {
                    let matched = self.label()?;
                    self.sequence(body, failure, matched)?;
                    dynasm!(self.ops; =>matched);
                }
                Node::Conditional { group, yes, no } => {
                    let otherwise = self.label()?;
                    dynasm!(self.ops; cmp QWORD [rsp + ((*group - 1) * 16) as i32], -1; je =>otherwise);
                    self.continuation(yes, &nodes[index + 1..], failure, success)?;
                    dynasm!(self.ops; =>otherwise);
                    return self.continuation(no, &nodes[index + 1..], failure, success);
                }
                Node::Start => dynasm!(self.ops; test r14, r14; jne =>failure),
                Node::End => dynasm!(self.ops; cmp r14, r13; jne =>failure),
                Node::Boundary { negated } => self.boundary(*negated, failure),
            }
        }
        dynasm!(self.ops; jmp =>success);
        Some(())
    }

    fn continuation(
        &mut self,
        branch: &[Node],
        rest: &[Node],
        failure: DynamicLabel,
        success: DynamicLabel,
    ) -> Option<()> {
        let nodes: Vec<_> = branch.iter().chain(rest).cloned().collect();
        self.sequence(&nodes, failure, success)
    }

    fn alternation(
        &mut self,
        branches: &[Vec<Node>],
        rest: &[Node],
        failure: DynamicLabel,
        success: DynamicLabel,
    ) -> Option<()> {
        let base = self.reserve()?;
        self.snapshot(base);
        dynasm!(self.ops; mov QWORD [rsp + base + 8], r14);
        for branch in branches {
            let retry = self.label()?;
            self.continuation(branch, rest, retry, success)?;
            dynasm!(self.ops; =>retry; mov r14, QWORD [rsp + base + 8]);
            self.restore(base);
            self.tick();
        }
        dynasm!(self.ops; jmp =>failure);
        Some(())
    }

    fn look(
        &mut self,
        body: &[Node],
        behind: usize,
        negated: bool,
        failure: DynamicLabel,
    ) -> Option<()> {
        let base = self.reserve()?;
        let matched = self.label()?;
        let missed = self.label()?;
        let finished = self.label()?;
        self.snapshot(base);
        dynasm!(self.ops; mov QWORD [rsp + base + 8], r14);
        for _ in 0..behind {
            self.tick();
            dynasm!(self.ops; test r14, r14; jz =>missed);
            self.back_one_character()?;
        }
        self.sequence(body, missed, matched)?;
        dynasm!(self.ops; =>matched);
        if behind > 0 {
            dynasm!(self.ops; cmp r14, QWORD [rsp + base + 8]; jne =>missed);
        }
        dynasm!(self.ops; mov r14, QWORD [rsp + base + 8]);
        if negated {
            self.restore(base);
            dynasm!(self.ops; jmp =>failure);
        } else {
            dynasm!(self.ops; jmp =>finished);
        }
        dynasm!(self.ops; =>missed; mov r14, QWORD [rsp + base + 8]);
        self.restore(base);
        let target = if negated { finished } else { failure };
        dynasm!(self.ops; jmp =>target; =>finished);
        Some(())
    }

    fn boundary(&mut self, negated: bool, failure: DynamicLabel) {
        let defer = self.defer;
        dynasm!(self.ops
            ; mov rdi, r12; mov rsi, r13; mov rdx, r14
            ; mov rax, QWORD super::word_boundary as *const () as i64; call rax
            ; cmp eax, 2; je =>defer; test eax, eax
        );
        if negated {
            dynasm!(self.ops; jnz =>failure);
        } else {
            dynasm!(self.ops; jz =>failure);
        }
    }

    fn atom(&mut self, atom: &Atom, failure: DynamicLabel) -> Option<()> {
        dynasm!(self.ops; cmp r14, r13; jae =>failure; movzx eax, BYTE [r12 + r14]);
        match atom {
            Atom::Class {
                ascii,
                non_ascii,
                ranges,
                ..
            } => {
                let unicode = if *non_ascii {
                    Some((self.label()?, self.label()?))
                } else {
                    None
                };
                let non_ascii_target = unicode.map_or(failure, |(entry, _)| entry);
                dynasm!(self.ops; cmp eax, 128; jae =>non_ascii_target);
                let count = ascii[0].count_ones() + ascii[1].count_ones();
                let first = if ascii[0] != 0 {
                    ascii[0].trailing_zeros()
                } else {
                    64 + ascii[1].trailing_zeros()
                };
                let last = if ascii[1] != 0 {
                    127 - ascii[1].leading_zeros()
                } else {
                    63 - ascii[0].leading_zeros().min(63)
                };
                if count == 1 {
                    dynasm!(self.ops; cmp eax, first as i32; jne =>failure);
                } else if count == 2 && (65..91).contains(&first) && last == first + 32 {
                    dynasm!(self.ops; or eax, 32; cmp eax, last as i32; jne =>failure);
                } else if count > 0 && count == last - first + 1 {
                    dynasm!(self.ops; sub eax, first as i32; cmp eax, count as i32; jae =>failure);
                } else {
                    let lower = self.label()?;
                    let test = self.label()?;
                    dynasm!(self.ops
                        ; cmp eax, 64; jb =>lower
                        ; mov rdx, QWORD ascii[1] as i64; jmp =>test
                        ; =>lower; mov rdx, QWORD ascii[0] as i64
                        ; =>test; bt rdx, rax; jnc =>failure
                    );
                }
                dynasm!(self.ops; inc r14);
                if let Some((entry, resume)) = unicode {
                    dynasm!(self.ops; =>resume);
                    self.unicode_stubs.push(UnicodeStub {
                        entry,
                        resume,
                        failure,
                        ranges: Arc::clone(ranges),
                    });
                }
            }
            Atom::Any { newline, crlf } => {
                if !newline {
                    dynasm!(self.ops; cmp eax, 10; je =>failure);
                    if *crlf {
                        dynasm!(self.ops; cmp eax, 13; je =>failure);
                    }
                }
                let one = self.label()?;
                let two = self.label()?;
                let three = self.label()?;
                let ready = self.label()?;
                dynasm!(self.ops
                    ; cmp eax, 128; jb =>one
                    ; cmp eax, 224; jb =>two
                    ; cmp eax, 240; jb =>three
                    ; mov ecx, 4; jmp =>ready
                    ; =>one; mov ecx, 1; jmp =>ready
                    ; =>two; mov ecx, 2; jmp =>ready
                    ; =>three; mov ecx, 3
                    ; =>ready; add r14, rcx
                    ; cmp r14, r13; ja =>self.defer
                );
            }
        }
        Some(())
    }

    fn backref(&mut self, group: usize, casei: bool, failure: DynamicLabel) -> Option<()> {
        let lo = ((group - 1) * 16) as i32;
        let hi = lo + 8;
        let head = self.label()?;
        let advance = self.label()?;
        let done = self.label()?;
        dynasm!(self.ops
            ; mov r8, QWORD [rsp + lo]; mov r9, QWORD [rsp + hi]
            ; cmp r8, -1; je =>failure; cmp r9, -1; je =>failure
            ; sub r9, r8
            ; mov rax, r13; sub rax, r14; cmp r9, rax; ja =>failure
            ; xor r10d, r10d
            ; =>head; cmp r10, r9; jae =>done
        );
        self.tick();
        dynasm!(self.ops
            ; lea rax, [r8 + r10]; movzx ecx, BYTE [r12 + rax]
            ; lea rax, [r14 + r10]; movzx edx, BYTE [r12 + rax]
            ; cmp ecx, edx
        );
        if casei {
            let unicode = self.label()?;
            let defer = self.defer;
            dynasm!(self.ops
                ; je =>advance
                ; mov eax, ecx; or eax, edx; test eax, 128; jnz =>unicode
                ; or ecx, 32; or edx, 32
                ; mov eax, ecx; sub eax, 97; cmp eax, 25; ja =>failure
                ; cmp ecx, edx; jne =>failure
                ; jmp =>advance
                ; =>unicode
                ; cmp r9, r15; ja =>defer; sub r15, r9
                ; mov rdi, r12; mov rsi, r13; mov rdx, r14; mov rcx, r8; mov r8, r9
                ; mov rax, QWORD super::unicode_backref as *const () as i64; call rax
                ; test eax, eax; jz =>failure
                ; mov r9, QWORD [rsp + hi]; sub r9, QWORD [rsp + lo]; jmp =>done
            );
        } else {
            dynasm!(self.ops; jne =>failure);
        }
        dynasm!(self.ops; =>advance; inc r10; jmp =>head; =>done; add r14, r9);
        Some(())
    }

    fn snapshot(&mut self, base: i32) {
        for slot in 0..self.slots {
            dynasm!(self.ops
                ; mov rax, QWORD [rsp + (slot * 8) as i32]
                ; mov QWORD [rsp + base + 24 + (slot * 8) as i32], rax
            );
        }
    }

    fn restore(&mut self, base: i32) {
        for slot in 0..self.slots {
            dynasm!(self.ops
                ; mov rax, QWORD [rsp + base + 24 + (slot * 8) as i32]
                ; mov QWORD [rsp + (slot * 8) as i32], rax
            );
        }
    }

    fn back_one_character(&mut self) -> Option<()> {
        let back = self.label()?;
        // Call sites establish that at least one whole character was consumed.
        dynasm!(self.ops
            ; =>back; dec r14; movzx eax, BYTE [r12 + r14]
            ; and eax, 192; cmp eax, 128; je =>back
        );
        Some(())
    }

    fn repeat(
        &mut self,
        atom: &Atom,
        repetition: Repetition,
        rest: &[Node],
        failure: DynamicLabel,
        success: DynamicLabel,
    ) -> Option<()> {
        let Repetition { lo, hi, greedy } = repetition;
        let base = self.reserve()?;
        self.snapshot(base);
        if greedy
            && lo == 0
            && hi == usize::MAX
            && let Atom::Any { newline, crlf } = atom
        {
            return self.repeat_any(*newline, *crlf, base, rest, failure, success);
        }
        let scan = self.label()?;
        let attempt = self.label()?;
        let retry = self.label()?;
        let target = if greedy { hi } else { lo };
        dynasm!(self.ops
            ; mov QWORD [rsp + base], 0
            ; =>scan; mov rax, QWORD target as i64
            ; cmp QWORD [rsp + base], rax; jae =>attempt
        );
        self.tick();
        self.atom(atom, if greedy { attempt } else { failure })?;
        dynasm!(self.ops; inc QWORD [rsp + base]; jmp =>scan; =>attempt
            ; mov rax, QWORD lo as i64; cmp QWORD [rsp + base], rax; jb =>failure);
        self.restore(base);
        dynasm!(self.ops; mov QWORD [rsp + base + 8], r14);
        self.sequence(rest, retry, success)?;
        dynasm!(self.ops; =>retry);
        self.tick();
        dynasm!(self.ops; mov r14, QWORD [rsp + base + 8]);
        if greedy {
            dynasm!(self.ops; mov rax, QWORD lo as i64; cmp QWORD [rsp + base], rax; jbe =>failure);
            self.back_one_character()?;
            dynasm!(self.ops; dec QWORD [rsp + base]);
        } else {
            dynasm!(self.ops; mov rax, QWORD hi as i64; cmp QWORD [rsp + base], rax; jae =>failure);
            self.atom(atom, failure)?;
            dynasm!(self.ops; inc QWORD [rsp + base]);
        }
        dynasm!(self.ops; jmp =>attempt);
        Some(())
    }

    fn repeat_any(
        &mut self,
        newline: bool,
        crlf: bool,
        base: i32,
        rest: &[Node],
        failure: DynamicLabel,
        success: DynamicLabel,
    ) -> Option<()> {
        let attempt = self.label()?;
        let retry = self.label()?;
        let defer = self.defer;
        dynasm!(self.ops; mov QWORD [rsp + base + 16], r14);
        if newline {
            dynasm!(self.ops; mov rax, r13);
        } else {
            dynasm!(self.ops
                ; mov rdi, r12; mov rsi, r13; mov rdx, r14; mov rcx, QWORD i64::from(crlf)
                ; mov rax, QWORD super::scan_any as *const () as i64; call rax
            );
        }
        dynasm!(self.ops
            ; mov rcx, rax; sub rcx, r14; cmp rcx, r15; ja =>defer
            ; sub r15, rcx; mov r14, rax
            ; =>attempt
        );
        self.restore(base);
        dynasm!(self.ops; mov QWORD [rsp + base + 8], r14);
        self.sequence(rest, retry, success)?;
        dynasm!(self.ops; =>retry);
        self.tick();
        dynasm!(self.ops
            ; mov r14, QWORD [rsp + base + 8]
            ; cmp r14, QWORD [rsp + base + 16]; jbe =>failure
        );
        self.back_one_character()?;
        dynasm!(self.ops; jmp =>attempt);
        Some(())
    }

    fn repeat_group(
        &mut self,
        body: &[Node],
        width: usize,
        repetition: Repetition,
        rest: &[Node],
        failure: DynamicLabel,
        success: DynamicLabel,
    ) -> Option<()> {
        let Repetition { lo, hi, greedy } = repetition;
        let base = self.reserve()?;
        self.snapshot(base);
        let scan = self.label()?;
        let consume = self.label()?;
        let consumed = self.label()?;
        let missed = self.label()?;
        let attempt = self.label()?;
        let retry = self.label()?;
        let target = if greedy { hi } else { lo };
        dynasm!(self.ops; mov QWORD [rsp + base], 0; mov QWORD [rsp + base + 8], r14
            ; =>scan; mov rax, QWORD target as i64; cmp QWORD [rsp + base], rax; jae =>attempt
            ; =>consume);
        self.tick();
        self.sequence(body, missed, consumed)?;
        dynasm!(self.ops; =>consumed; inc QWORD [rsp + base]; mov QWORD [rsp + base + 8], r14);
        if greedy {
            dynasm!(self.ops; jmp =>scan);
        } else {
            dynasm!(self.ops; mov rax, QWORD lo as i64; cmp QWORD [rsp + base], rax; jb =>scan; jmp =>attempt);
        }
        dynasm!(self.ops; =>missed; mov r14, QWORD [rsp + base + 8]);
        let missed_target = if greedy { attempt } else { failure };
        dynasm!(self.ops; jmp =>missed_target; =>attempt
            ; mov rax, QWORD lo as i64; cmp QWORD [rsp + base], rax; jb =>failure);
        self.restore(base);
        // Every capture in this deterministic body is written on every
        // iteration. Replay just the last iteration after restoring the outer
        // state, rather than retaining an unbounded capture-history stack.
        if body.iter().any(|node| matches!(node, Node::Save(_))) {
            let ready = self.label()?;
            dynasm!(self.ops; cmp QWORD [rsp + base], 0; je =>ready);
            for _ in 0..width {
                self.tick();
                self.back_one_character()?;
            }
            self.sequence(body, self.defer, ready)?;
            dynasm!(self.ops; =>ready);
        }
        self.sequence(rest, retry, success)?;
        dynasm!(self.ops; =>retry);
        self.tick();
        dynasm!(self.ops; mov r14, QWORD [rsp + base + 8]);
        if greedy {
            dynasm!(self.ops; mov rax, QWORD lo as i64; cmp QWORD [rsp + base], rax; jbe =>failure);
            for _ in 0..width {
                self.tick();
                self.back_one_character()?;
            }
            dynasm!(self.ops; dec QWORD [rsp + base]; mov QWORD [rsp + base + 8], r14; jmp =>attempt);
        } else {
            dynasm!(self.ops; mov rax, QWORD hi as i64; cmp QWORD [rsp + base], rax; jae =>failure; jmp =>consume);
        }
        Some(())
    }
}
