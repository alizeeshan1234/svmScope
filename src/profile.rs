//! Compute profiler: where every BPF instruction of a transaction went.
//!
//! LiteSVM's register tracing records every instruction each program frame
//! executes, including CPIs. Walking that trace with the executable's static
//! analysis (function boundaries from the call graph, syscall names from the
//! loader) attributes instructions to functions and to syscalls, and folds the
//! call stacks into flamegraph input.
//!
//! Counts are *executed BPF instructions*, which cost one compute unit each.
//! Syscalls cost extra per call (their published prices); they are counted
//! separately so the two can be combined by the caller.
//!
//! Mainnet programs are stripped: only `entrypoint` and `custom_panic` keep
//! names, every other function is `function_<pc>`. Function *boundaries* are
//! still exact, so the profile's shape is exact; names come from an unstripped
//! build of the same program when the caller has one (see
//! [`Profile::symbolize`]).

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use litesvm::{InvocationInspectCallback, LiteSVM};
use serde::Serialize;
use solana_program_runtime::invoke_context::{Executable, InvokeContext, RegisterTrace};
use solana_program_runtime::solana_sbpf::{ebpf, static_analysis::Analysis};
use solana_transaction::sanitized::SanitizedTransaction;
use solana_transaction_context::instruction::InstructionContext;
use solana_transaction_context::IndexOfAccount;

/// One function's share of a frame.
#[derive(Debug, Clone, Serialize)]
pub struct FunctionProfile {
    /// `entrypoint`, a symbol name, or `function_<pc>` for a stripped program.
    pub name: String,
    /// The function's first instruction.
    pub pc: usize,
    /// Instructions executed inside this function itself.
    pub self_insns: u64,
    /// Instructions executed inside this function and everything it called.
    pub total_insns: u64,
    /// Times it was entered.
    pub calls: u64,
    /// Estimated compute units: the frame's measured CU split across its
    /// functions in proportion to `self_insns`. Exact when the frame made no
    /// syscalls (one CU per instruction); otherwise the syscall overhead is
    /// spread proportionally. `None` until [`Profile`] has frame compute.
    pub compute_units: Option<u64>,
}

/// One program frame (a top-level instruction or a CPI) of the transaction.
#[derive(Debug, Clone, Serialize)]
pub struct FrameProfile {
    /// The program that ran.
    pub program: String,
    /// Total BPF instructions the frame executed.
    pub instructions: u64,
    /// Compute units the runtime charged this frame (its `consumed` log line).
    /// `None` when the logs did not report it.
    pub compute_units: Option<u64>,
    /// `compute_units - instructions`: what syscalls, CPI overhead and
    /// account serialization cost beyond one CU per BPF instruction.
    pub syscall_overhead: Option<u64>,
    /// Per-function breakdown, largest `self_insns` first.
    pub functions: Vec<FunctionProfile>,
    /// Syscall name → number of calls.
    pub syscalls: Vec<(String, u64)>,
    /// Folded stacks (`a;b;c`) → instructions, the flamegraph input.
    pub stacks: Vec<(String, u64)>,
}

/// The whole transaction's profile: one entry per program frame, in
/// execution order.
#[derive(Debug, Clone, Serialize)]
pub struct Profile {
    /// One entry per program frame, in execution order.
    pub frames: Vec<FrameProfile>,
}

impl Profile {
    /// Attach the runtime's measured compute to each frame from the
    /// transaction's logs. Frames and `Program X consumed` spans are both in
    /// invocation order; builtins (System, precompiles) leave no trace, so
    /// spans are matched to frames by program id and skipped otherwise.
    pub(crate) fn attach_compute(&mut self, logs: &[String]) {
        let spans = crate::trace::spans_from_logs(logs, 0);
        let mut next = 0usize;
        for span in &spans {
            let Some(frame) = self.frames.get_mut(next) else {
                break;
            };
            if frame.program != span.program {
                continue; // a builtin or precompile: no BPF trace for it
            }
            if let Some(cu) = span.cu_consumed {
                frame.compute_units = Some(cu);
                frame.syscall_overhead = Some(cu.saturating_sub(frame.instructions));
                let insns = frame.instructions.max(1);
                for f in &mut frame.functions {
                    f.compute_units = Some(cu * f.self_insns / insns);
                }
            }
            next += 1;
        }
    }

    /// Name a program's functions from the unstripped ELF of the *same* build
    /// — the `.debug` file `cargo build-sbf --debug` writes next to the `.so`.
    /// Symbol addresses are checked against the program's actual entrypoint
    /// so a mismatched build is refused rather than mislabelled.
    ///
    /// Renames every `function_<pc>` in that program's frames, functions and
    /// folded stacks; Rust symbols are demangled.
    pub fn symbolize(&mut self, program: &str, elf_with_symbols: &[u8]) -> crate::Result<usize> {
        let symbols = elf_function_symbols(elf_with_symbols)
            .ok_or_else(|| crate::Error::InvalidSpec("not an ELF with a symbol table".into()))?;
        let mut renamed = 0usize;
        for frame in self.frames.iter_mut().filter(|f| f.program == program) {
            // Self-check: the ELF's `entrypoint` must sit where this program's
            // actual entrypoint ran.
            if let Some(entry) = frame.functions.iter().find(|f| f.name == "entrypoint") {
                match symbols.get(&entry.pc) {
                    Some(n) if n == "entrypoint" => {}
                    _ => {
                        return Err(crate::Error::InvalidSpec(format!(
                            "symbols do not match program {program}: its entrypoint runs at pc {} but the ELF's entrypoint symbol does not",
                            entry.pc
                        )))
                    }
                }
            }
            let mut rename: BTreeMap<String, String> = BTreeMap::new();
            for f in &mut frame.functions {
                if let Some(sym) = symbols.get(&f.pc) {
                    let pretty = rustc_demangle::demangle(sym).to_string();
                    let pretty = strip_hash(&pretty);
                    if pretty != f.name {
                        rename.insert(f.name.clone(), pretty.clone());
                        f.name = pretty;
                        renamed += 1;
                    }
                }
            }
            for (stack, _) in &mut frame.stacks {
                *stack = stack
                    .split(';')
                    .map(|s| rename.get(s).cloned().unwrap_or_else(|| s.to_string()))
                    .collect::<Vec<_>>()
                    .join(";");
            }
        }
        Ok(renamed)
    }

    /// Total BPF instructions across every frame.
    pub fn instructions(&self) -> u64 {
        self.frames.iter().map(|f| f.instructions).sum()
    }

    /// Instructions per program, summed across that program's frames.
    pub fn by_program(&self) -> Vec<(String, u64)> {
        let mut m: BTreeMap<String, u64> = BTreeMap::new();
        for f in &self.frames {
            *m.entry(f.program.clone()).or_default() += f.instructions;
        }
        let mut v: Vec<_> = m.into_iter().collect();
        v.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
        v
    }
}

/// `FUNC` symbols of an ELF64 (its `.symtab`, falling back to `.dynsym`),
/// keyed by sBPF program counter: `(st_value - .text address) / 8`.
fn elf_function_symbols(elf: &[u8]) -> Option<BTreeMap<usize, String>> {
    if elf.get(0..4)? != b"\x7fELF" {
        return None;
    }
    let u16_at =
        |o: usize| -> Option<u16> { Some(u16::from_le_bytes(elf.get(o..o + 2)?.try_into().ok()?)) };
    let u32_at =
        |o: usize| -> Option<u32> { Some(u32::from_le_bytes(elf.get(o..o + 4)?.try_into().ok()?)) };
    let u64_at =
        |o: usize| -> Option<u64> { Some(u64::from_le_bytes(elf.get(o..o + 8)?.try_into().ok()?)) };
    let shoff = u64_at(0x28)? as usize;
    let shentsize = u16_at(0x3a)? as usize;
    let shnum = u16_at(0x3c)? as usize;
    let shstrndx = u16_at(0x3e)? as usize;
    // (name, type, addr, offset, size, link, entsize)
    let section = |i: usize| -> Option<(u32, u32, u64, usize, usize, usize, usize)> {
        let o = shoff.checked_add(i.checked_mul(shentsize)?)?;
        Some((
            u32_at(o)?,
            u32_at(o + 4)?,
            u64_at(o + 16)?,
            u64_at(o + 24)? as usize,
            u64_at(o + 32)? as usize,
            u32_at(o + 40)? as usize,
            u64_at(o + 56)? as usize,
        ))
    };
    let cstr = |base: usize, off: usize| -> Option<String> {
        let start = base.checked_add(off)?;
        let end = start + elf.get(start..)?.iter().position(|&b| b == 0)?;
        Some(String::from_utf8_lossy(&elf[start..end]).to_string())
    };
    let (_, _, _, shstr_off, _, _, _) = section(shstrndx)?;
    let mut text_addr = None;
    for i in 0..shnum {
        let (name, _, addr, _, _, _, _) = section(i)?;
        if cstr(shstr_off, name as usize)? == ".text" {
            text_addr = Some(addr);
        }
    }
    let text_addr = text_addr?;
    const SHT_SYMTAB: u32 = 2;
    const SHT_DYNSYM: u32 = 11;
    const STT_FUNC: u8 = 2;
    let mut out = BTreeMap::new();
    for wanted in [SHT_SYMTAB, SHT_DYNSYM] {
        for i in 0..shnum {
            let (_, typ, _, off, size, link, entsize) = section(i)?;
            if typ != wanted || entsize == 0 {
                continue;
            }
            let (_, _, _, str_off, _, _, _) = section(link)?;
            for j in 0..size / entsize {
                let e = off + j * entsize;
                let st_name = u32_at(e)? as usize;
                let st_info = *elf.get(e + 4)?;
                let st_value = u64_at(e + 8)?;
                if st_info & 0xf != STT_FUNC || st_name == 0 || st_value < text_addr {
                    continue;
                }
                let pc = ((st_value - text_addr) / 8) as usize;
                out.entry(pc).or_insert(cstr(str_off, st_name)?);
            }
        }
        if !out.is_empty() {
            break;
        }
    }
    Some(out)
}

/// `foo::bar::h9a99872dbe52d553` → `foo::bar`.
fn strip_hash(name: &str) -> String {
    match name.rsplit_once("::h") {
        Some((head, hash)) if hash.len() == 16 && hash.bytes().all(|b| b.is_ascii_hexdigit()) => {
            head.to_string()
        }
        _ => name.to_string(),
    }
}

/// Captures every frame's register trace as the transaction executes.
struct Collector {
    frames: Arc<Mutex<Vec<FrameProfile>>>,
}

impl InvocationInspectCallback for Collector {
    fn before_invocation(
        &self,
        _svm: &LiteSVM,
        _tx: &SanitizedTransaction,
        _program_indices: &[IndexOfAccount],
        _invoke_context: &mut InvokeContext,
        _enable_register_tracing: bool,
    ) {
    }

    fn after_invocation(
        &self,
        _svm: &LiteSVM,
        _tx: &SanitizedTransaction,
        _program_indices: &[IndexOfAccount],
        invoke_context: &InvokeContext,
        enable_register_tracing: bool,
    ) {
        if !enable_register_tracing {
            return;
        }
        invoke_context.iterate_vm_traces(
            &|ictx: InstructionContext, exe: &Executable, trace: RegisterTrace| {
                let program = ictx
                    .get_program_key()
                    .map(|k| k.to_string())
                    .unwrap_or_default();
                if let Some(frame) = profile_frame(program, exe, &trace) {
                    self.frames.lock().unwrap().push(frame);
                }
            },
        );
    }
}

/// Attribute one frame's trace to functions, syscalls and folded stacks.
fn profile_frame(program: String, exe: &Executable, trace: &RegisterTrace) -> Option<FrameProfile> {
    if trace.is_empty() {
        return None;
    }
    let analysis = Analysis::from_executable(exe).ok()?;
    let functions: &BTreeMap<usize, (u32, String)> = &analysis.functions;
    let (_, text) = exe.get_text_bytes();
    let static_syscalls = exe.get_sbpf_version().static_syscalls();
    let loader = exe.get_loader();
    let syscall_registry = loader.get_function_registry();
    let program_registry = exe.get_function_registry();

    let enclosing = |pc: usize| -> usize {
        functions
            .range(..=pc)
            .next_back()
            .map(|(s, _)| *s)
            .unwrap_or(0)
    };
    let name_of = |start: usize| -> String {
        functions
            .get(&start)
            .map(|(_, n)| n.clone())
            .unwrap_or_else(|| format!("function_{start}"))
    };

    let mut self_insns: BTreeMap<usize, u64> = BTreeMap::new();
    let mut total_insns: BTreeMap<usize, u64> = BTreeMap::new();
    let mut calls: BTreeMap<usize, u64> = BTreeMap::new();
    let mut syscalls: BTreeMap<String, u64> = BTreeMap::new();
    let mut stacks: BTreeMap<String, u64> = BTreeMap::new();

    let first_pc = trace[0][11] as usize;
    let mut stack: Vec<usize> = vec![enclosing(first_pc)];
    *calls.entry(stack[0]).or_default() += 1;

    for regs in trace.iter() {
        let pc = regs[11] as usize;
        // Keep the stack honest against the trace: after a `callx` or an
        // unmatched exit, the enclosing function of the current pc wins.
        let here = enclosing(pc);
        match stack.last() {
            Some(&top) if top == here => {}
            _ => {
                if let Some(pos) = stack.iter().rposition(|&f| f == here) {
                    stack.truncate(pos + 1);
                } else {
                    stack.push(here);
                    *calls.entry(here).or_default() += 1;
                }
            }
        }
        *self_insns.entry(here).or_default() += 1;
        for &f in &stack {
            *total_insns.entry(f).or_default() += 1;
        }
        let key: Vec<String> = stack.iter().map(|&f| name_of(f)).collect();
        *stacks.entry(key.join(";")).or_default() += 1;

        let insn = ebpf::get_insn_unchecked(text, pc);
        match insn.opc {
            ebpf::CALL_IMM => {
                // Same resolution order as the interpreter: syscall first.
                if !static_syscalls || insn.src == 0 {
                    if let Some((name, _)) = syscall_registry.lookup_by_key(insn.imm as u32) {
                        let name = String::from_utf8_lossy(name).to_string();
                        *syscalls.entry(name).or_default() += 1;
                        continue;
                    }
                }
                let target = if static_syscalls {
                    (insn.src == 1).then(|| (pc as i64 + 1 + insn.imm) as usize)
                } else {
                    program_registry
                        .lookup_by_key(insn.imm as u32)
                        .map(|(_, t)| t)
                };
                if let Some(t) = target {
                    stack.push(t);
                    *calls.entry(t).or_default() += 1;
                }
            }
            ebpf::EXIT => {
                stack.pop();
            }
            _ => {}
        }
    }

    let mut fns: Vec<FunctionProfile> = self_insns
        .iter()
        .map(|(&pc, &s)| FunctionProfile {
            name: name_of(pc),
            pc,
            self_insns: s,
            total_insns: *total_insns.get(&pc).unwrap_or(&s),
            calls: *calls.get(&pc).unwrap_or(&0),
            compute_units: None,
        })
        .collect();
    fns.sort_by_key(|f| std::cmp::Reverse(f.self_insns));
    let mut sys: Vec<_> = syscalls.into_iter().collect();
    sys.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
    let mut folded: Vec<_> = stacks.into_iter().collect();
    folded.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
    Some(FrameProfile {
        program,
        instructions: trace.len() as u64,
        compute_units: None,
        syscall_overhead: None,
        functions: fns,
        syscalls: sys,
        stacks: folded,
    })
}

impl crate::Replay {
    /// Profile the transaction: trace every BPF instruction it executes and
    /// attribute them to functions, syscalls and call stacks, per program
    /// frame. `mutations` apply first, as in [`Self::simulate`].
    pub fn profile(
        &self,
        mutations: &[crate::Mutation],
    ) -> crate::Result<(crate::ReplayResult, Profile)> {
        let mut svm = self.ctx.fresh_svm_with(true);
        let frames = Arc::new(Mutex::new(Vec::new()));
        svm.set_invocation_inspect_callback(Collector {
            frames: Arc::clone(&frames),
        });
        self.ctx.apply_mutations_to(&mut svm, mutations)?;
        let tx = self.ctx.tx_for(mutations)?;
        let result = crate::replay::replay_result_of(&svm.send_transaction(tx));
        let frames = std::mem::take(&mut *frames.lock().unwrap());
        let mut profile = Profile { frames };
        profile.attach_compute(&result.logs);
        Ok((result, profile))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal ELF64 with a `.text` at 0x120 and a `.symtab` holding two
    /// FUNC symbols, built by hand so the parser is tested without fixtures.
    fn tiny_elf() -> Vec<u8> {
        let mut elf = vec![0u8; 0x40];
        elf[..4].copy_from_slice(b"\x7fELF");
        elf[4] = 2; // 64-bit
        elf[5] = 1; // little-endian
                    // Layout: [header 0x40][.text 0x120..0x160][.strtab][.symtab][.shstrtab][section headers]
        elf.resize(0x120, 0);
        elf.extend_from_slice(&[0u8; 64]); // 8 instructions of .text at addr 0x120
        let strtab_off = elf.len();
        elf.extend_from_slice(b"\0entrypoint\0_ZN3foo3bar17h9a99872dbe52d553E\0");
        let symtab_off = elf.len();
        let mut sym = |name: u32, value: u64| {
            elf.extend_from_slice(&name.to_le_bytes());
            elf.push(2); // STT_FUNC
            elf.push(0);
            elf.extend_from_slice(&1u16.to_le_bytes());
            elf.extend_from_slice(&value.to_le_bytes());
            elf.extend_from_slice(&8u64.to_le_bytes());
        };
        sym(0, 0); // null symbol
        sym(1, 0x120); // entrypoint at pc 0
        sym(12, 0x120 + 3 * 8); // foo::bar at pc 3
        let shstr_off = elf.len();
        elf.extend_from_slice(b"\0.text\0.strtab\0.symtab\0.shstrtab\0");
        let shoff = elf.len();
        let mut sh =
            |name: u32, typ: u32, addr: u64, off: u64, size: u64, link: u32, entsize: u64| {
                elf.extend_from_slice(&name.to_le_bytes());
                elf.extend_from_slice(&typ.to_le_bytes());
                elf.extend_from_slice(&0u64.to_le_bytes()); // flags
                elf.extend_from_slice(&addr.to_le_bytes());
                elf.extend_from_slice(&off.to_le_bytes());
                elf.extend_from_slice(&size.to_le_bytes());
                elf.extend_from_slice(&link.to_le_bytes());
                elf.extend_from_slice(&0u32.to_le_bytes()); // info
                elf.extend_from_slice(&0u64.to_le_bytes()); // align
                elf.extend_from_slice(&entsize.to_le_bytes());
            };
        sh(0, 0, 0, 0, 0, 0, 0);
        sh(1, 1, 0x120, 0x120, 64, 0, 0); // .text
        sh(
            7,
            3,
            0,
            strtab_off as u64,
            (symtab_off - strtab_off) as u64,
            0,
            0,
        ); // .strtab
        sh(
            15,
            2,
            0,
            symtab_off as u64,
            (shstr_off - symtab_off) as u64,
            2,
            24,
        ); // .symtab → link 2
        sh(23, 3, 0, shstr_off as u64, (shoff - shstr_off) as u64, 0, 0); // .shstrtab
        elf[0x28..0x30].copy_from_slice(&(shoff as u64).to_le_bytes());
        elf[0x3a..0x3c].copy_from_slice(&64u16.to_le_bytes());
        elf[0x3c..0x3e].copy_from_slice(&5u16.to_le_bytes());
        elf[0x3e..0x40].copy_from_slice(&4u16.to_le_bytes());
        elf
    }

    fn frame(program: &str, fns: &[(usize, &str, u64)]) -> FrameProfile {
        FrameProfile {
            program: program.into(),
            instructions: fns.iter().map(|f| f.2).sum(),
            compute_units: None,
            syscall_overhead: None,
            functions: fns
                .iter()
                .map(|&(pc, name, n)| FunctionProfile {
                    name: name.into(),
                    pc,
                    self_insns: n,
                    total_insns: n,
                    calls: 1,
                    compute_units: None,
                })
                .collect(),
            syscalls: vec![],
            stacks: vec![(fns.iter().map(|f| f.1).collect::<Vec<_>>().join(";"), 1)],
        }
    }

    #[test]
    fn elf_symbols_map_to_program_counters() {
        let syms = elf_function_symbols(&tiny_elf()).unwrap();
        assert_eq!(syms.get(&0).map(String::as_str), Some("entrypoint"));
        assert_eq!(
            syms.get(&3).map(String::as_str),
            Some("_ZN3foo3bar17h9a99872dbe52d553E")
        );
        assert!(elf_function_symbols(b"not an elf").is_none());
    }

    #[test]
    fn symbolize_renames_functions_and_stacks_and_refuses_a_mismatch() {
        let mut p = Profile {
            frames: vec![frame("P", &[(0, "entrypoint", 10), (3, "function_3", 90)])],
        };
        let renamed = p.symbolize("P", &tiny_elf()).unwrap();
        assert_eq!(renamed, 1);
        assert_eq!(p.frames[0].functions[1].name, "foo::bar");
        assert_eq!(p.frames[0].stacks[0].0, "entrypoint;foo::bar");
        // Entrypoint at a different pc than the ELF says: refused, untouched.
        let mut q = Profile {
            frames: vec![frame("P", &[(5, "entrypoint", 10), (3, "function_3", 90)])],
        };
        assert!(q.symbolize("P", &tiny_elf()).is_err());
        assert_eq!(q.frames[0].functions[1].name, "function_3");
        assert_eq!(strip_hash("a::b::h0123456789abcdef"), "a::b");
        assert_eq!(strip_hash("a::b::hxyz"), "a::b::hxyz");
    }

    #[test]
    fn compute_attaches_by_invocation_order_skipping_builtins() {
        let mut p = Profile {
            frames: vec![
                frame("Tok", &[(0, "entrypoint", 100)]),
                frame("Amm", &[(0, "entrypoint", 300), (9, "function_9", 700)]),
            ],
        };
        let logs: Vec<String> = [
            "Program 11111111111111111111111111111111 invoke [1]",
            "Program 11111111111111111111111111111111 success",
            "Program Tok invoke [1]",
            "Program Tok consumed 150 of 200 compute units",
            "Program Tok success",
            "Program Amm invoke [1]",
            "Program Amm consumed 2000 of 10000 compute units",
            "Program Amm success",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        p.attach_compute(&logs);
        assert_eq!(p.frames[0].compute_units, Some(150));
        assert_eq!(p.frames[0].syscall_overhead, Some(50));
        assert_eq!(p.frames[1].compute_units, Some(2000));
        assert_eq!(
            p.frames[1].functions[1].compute_units,
            Some(1400),
            "700 of 1000 insns → 70% of 2000 CU"
        );
    }
}
