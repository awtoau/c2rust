#![deny(missing_docs)]
//! This module provides basic support for converting inline assembly statements.

use std::fmt::{self as fmt, Display, Formatter};

use crate::diagnostics::TranslationResult;

use super::*;
use log::warn;
use proc_macro2::{TokenStream, TokenTree};
use syn::__private::ToTokens;

/// An argument direction specifier for a Rust asm! expression
enum ArgDirSpec {
    In,
    Out,
    InOut,
    LateOut,
    InLateOut,
}

impl Display for ArgDirSpec {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        use ArgDirSpec::*;
        write!(
            f,
            "{}",
            match self {
                In => "in",
                Out => "out",
                InOut => "inout",
                LateOut => "lateout",
                InLateOut => "inlateout",
            }
        )
    }
}

impl ArgDirSpec {
    fn with_in(&self) -> Self {
        use ArgDirSpec::*;
        match self {
            In => In,
            Out => InOut,
            InOut => InOut,
            LateOut => InLateOut,
            InLateOut => InLateOut,
        }
    }
}

/// A machine architecture that rustc inline assembly knows about
#[derive(Copy, Clone, PartialEq)]
enum Arch {
    X86,
    X86_64,
    Arm,
    Aarch64,
    Riscv,
}

/// Parse a machine architecture from a target tuple. This is a best-effort attempt.
fn parse_arch(target_tuple: &str) -> Option<Arch> {
    if target_tuple.starts_with("x86_64") {
        Some(Arch::X86_64)
    } else if target_tuple.starts_with("i386")
        || target_tuple.starts_with("i486")
        || target_tuple.starts_with("i586")
        || target_tuple.starts_with("i686")
        || target_tuple.starts_with("x86")
    {
        Some(Arch::X86)
    } else if target_tuple.starts_with("aarch64")
        || target_tuple.starts_with("armv8")
        || target_tuple.starts_with("arm64")
    {
        Some(Arch::Aarch64)
    } else if target_tuple.starts_with("arm") || target_tuple.starts_with("thumbv") {
        Some(Arch::Arm)
    } else if target_tuple.starts_with("riscv") {
        Some(Arch::Riscv)
    } else {
        None
    }
}

fn parse_constraints(
    mut constraints: &str,
    arch: Arch,
) -> TranslationResult<(ArgDirSpec, bool, String)> {
    let parse_error = |constraints| {
        Err(TranslationError::new(
            None,
            failure::err_msg(
                "Inline assembly constraints could not be parsed: ".to_owned() + constraints,
            )
            .context(TranslationErrorKind::Generic),
        ))
    };
    use ArgDirSpec::*;
    let mut is_input = match constraints.chars().next() {
        Some('+') => {
            constraints = &constraints[1..];
            true
        }
        Some('=') => {
            constraints = &constraints[1..];
            false
        }
        _ => true,
    };

    let early_clobber = if constraints.starts_with('&') {
        constraints = &constraints[1..];
        true
    } else {
        false
    };

    let mut mem_only = if constraints.starts_with('*') {
        constraints = &constraints[1..];
        true
    } else {
        false
    };

    let mut split = constraints.splitn(2, ',');
    constraints = match split.next() {
        Some(c) => c,
        // Parse error
        _ => return parse_error(constraints),
    };
    // If a comma is present, this is an output of form =[&]foo,N
    if split.next().is_some() {
        if !is_input {
            is_input = true;
        } else {
            // '+' followed by ',' is a parse error
            return parse_error(constraints);
        }
    }

    // Handle register names
    let constraints = constraints.replace(['{', '}'], "\"");
    let mut llvm_constraints = constraints.clone();
    let constraints = constraints.as_str();

    // GCC/Clang extended-asm constraint strings can list multiple
    // alternative letters for a single operand (e.g. RISC-V's "rK" means
    // "either any general register, or a 5-bit unsigned immediate", and
    // the kernel also has the reverse order "Jr"). The compiler is free to
    // pick whichever alternative it can satisfy for a given argument, and
    // `r` (and, similarly, `m`/`i`) is always satisfiable regardless of
    // argument order. So: scan the whole constraint string up front for
    // one of these arch-generic letters and prefer it outright, rather
    // than resolving letters left-to-right and letting a later, more
    // restrictive letter (e.g. a machine constraint that only applies to
    // compile-time-constant operands, like RISC-V's I/J/K) overwrite an
    // already-valid mapping - or get chosen first and reject an operand
    // that a plain register would have handled fine. This is what fixes a
    // real bug where `"rK"` used to resolve to `"reg"` on the `r`
    // alternative, then a left-to-right loop continued into `K` and
    // clobbered that with a passthrough of the raw, un-mapped GCC letter
    // (see translate_machine_constraint's Riscv arm below).
    if constraints.is_empty() {
        // Nothing to translate (matches prior behavior: an empty
        // constraint string is left as-is rather than treated as an
        // error).
    } else if let Some(generic) = constraints.chars().find(|c| matches!(c, 'm' | 'r' | 'i')) {
        if generic == 'm' {
            mem_only = true;
        }
        // For 'i', Rust inline assembly has no constraint for that, but
        // uses the argument as a register-held value anyway.
        llvm_constraints = "reg".into();
    } else {
        // No arch-generic fallback letter is present; the whole
        // constraint string must be a single register name/index or a
        // machine-specific constraint.
        let is_explicit_reg = constraints.starts_with('"');
        let is_tied = !constraints.contains(|c: char| !c.is_ascii_digit());

        if !(is_explicit_reg || is_tied) {
            // Attempt to parse machine-specific constraints
            if let Some((machine_constraints, is_mem)) =
                translate_machine_constraint(constraints, arch)
            {
                llvm_constraints = machine_constraints.into();
                mem_only = is_mem;
            } else {
                // Refuse to guess: passing the raw GCC/LLVM constraint
                // letter through as a Rust register-class name either
                // fails to compile (the common case - Rust rejects
                // unknown register classes outright) or, worse, could
                // coincide with a real Rust register-class/register name
                // and silently compile with the wrong operand handling.
                // Per this translator's "no output rather than wrong
                // output" policy for cases it cannot safely translate,
                // fail the whole statement with a clear error instead;
                // the caller (convert_asm) already propagates this as a
                // translation error for just the containing asm
                // statement, degrading gracefully rather than emitting
                // assembly whose semantics were never actually verified.
                warn!(
                    "Did not recognize inline asm constraint: {}\n\
                    This constraint has no known, safe translation to a Rust \
                    asm! register class or operand kind; the containing \
                    statement will be translated as an error instead of \
                    emitting output that may silently misbehave.",
                    constraints
                );
                return Err(TranslationError::new(
                    None,
                    failure::err_msg(format!(
                        "Inline assembly constraint '{constraints}' has no known \
                         translation to a Rust asm! operand kind for this target"
                    ))
                    .context(TranslationErrorKind::Generic),
                ));
            }
        }
    }

    let mode = if mem_only {
        In
    } else {
        match (is_input, early_clobber) {
            (false, false) => LateOut,
            (false, true) => Out,
            (true, false) => InLateOut,
            (true, true) => InOut,
        }
    };

    Ok((mode, mem_only, llvm_constraints))
}

fn is_regname_or_int(parsed_constraint: &str) -> bool {
    parsed_constraint.contains('"') || parsed_constraint.starts_with(|c: char| c.is_ascii_digit())
}

/// Translate an architecture-specific assembly constraint from llvm/gcc
/// to those accepted by the Rust asm! macro. "Simple" (arch-independent)
/// constraints are handled in `parse_constraints`, not here.
/// See <https://gcc.gnu.org/onlinedocs/gcc/Machine-Constraints.html>,
/// <https://llvm.org/docs/LangRef.html#constraint-codes>, and
/// <https://doc.rust-lang.org/nightly/reference/inline-assembly.html#register-operands>
fn translate_machine_constraint(constraint: &str, arch: Arch) -> Option<(&str, bool)> {
    let mem = &mut false;
    // Many constraints are not handled here, because rustc does. The best we can
    let constraint = match arch {
        Arch::X86 | Arch::X86_64 => match constraint {
            // "R" => "reg_word", // rust does not support this
            "Q" => "reg_abcd",
            "q" => "reg_byte",
            "a" => "\"a\"",
            "b" => "\"b\"",
            "c" => "\"c\"",
            "d" => "\"d\"",
            "S" => "\"si\"",
            "D" => "\"di\"",
            // "A" => "a_and_d", // rust does not support this
            "U" => {
                warn!(
                    "the x86 'U' inline assembly operand constraint cannot \
                be translated correctly. It corresponds to the `clobber_abi` \
                option for `asm!`, but c2rust does not know the ABI being \
                used, so it cannot be translated automatically. Please correct \
                manually after translation."
                );
                return None;
            }
            "f" => "x87_reg",
            "t" => "\"st(0)\"",
            "u" => "\"st(1)\"",
            "x" => "xmm_reg", // this could also translate as ymm_reg
            "y" => "mmx_reg",
            "v" => "zmm_reg",
            "Yz" => "\"xmm0\"",
            "Yk" => "kreg",

            _ => return None,
        },
        Arch::Aarch64 => match constraint {
            "k" => "\"SP\"",
            "w" => "vreg",
            "x" => "vreg_low16",
            // "y" => "vreg_low8", // rust does not support this
            // "Upl" => "preg_low8", // rust does not support this
            "Upa" => "preg",
            "Q" => {
                *mem = true;
                "reg"
            }
            "Ump" => {
                *mem = true;
                "reg"
            }
            _ => return None,
        },
        Arch::Arm => match constraint {
            // "h" => "reg_8_15", // rust does not support this
            "k" => "\"SP\"",
            "l" => "reg",
            "t" => "sreg",
            "x" => "sreg_low16",
            "w" => "dreg",
            // "y" => "sreg",
            // "z" => "sreg",
            "Q" => {
                *mem = true;
                "reg"
            }
            "Uv" | "Uy" | "Uq" => {
                *mem = true;
                "reg"
            }
            _ => return None,
        },
        Arch::Riscv => match constraint {
            "f" => "freg",
            // "A": "an address that is held in a general-purpose register"
            // (GCC RISC-V machine constraints, config/riscv/constraints.md).
            // This is used on the memory operand of AMO (atomic memory
            // operation) instructions like `amoand.d`/`amoswap.d`/`lr.w`,
            // where the asm template dereferences the operand directly
            // (`amoand.d zero, {1}, {0}` etc. with no offset/indexing), so
            // it maps to a plain GPR used as a memory address, i.e. Rust's
            // `reg` register class with the mem-only (address-of) handling
            // that `parse_constraints`'s caller applies via `is_mem`.
            "A" => {
                *mem = true;
                "reg"
            }
            // "I"/"J"/"K" are pure immediate-value constraints (12-bit
            // signed immediate / integer zero / 5-bit unsigned CSR
            // immediate, respectively) with no register-class meaning at
            // all: GCC only ever uses them as one alternative in a
            // multi-letter constraint string (typically "rI"/"rJ"/"rK"),
            // where the other alternative, "r" (any GPR), is always a
            // valid fallback and is what `parse_constraints` now resolves
            // to before this function is ever reached for those cases (see
            // the early `break` on the 'r' arm above). If we get here with
            // a *lone* immediate-only letter, the operand's value is a
            // compile-time-unknown expression from the translator's point
            // of view, and Rust's `asm!` immediates (`const` operands)
            // require operands to be actual compile-time constants - not
            // just "was a constant when GCC compiled the original C". There
            // is no way to soundly force that here, so refuse to guess and
            // let the caller fall through to its existing
            // recognize-failure path, which raises a clear translation
            // error for the containing statement instead of emitting an
            // asm! that either fails to build or silently mis-encodes the
            // operand.
            "I" | "J" | "K" => return None,
            _ => return None,
        },
    };
    Some((constraint, *mem))
}

/// Translate a template modifier from llvm/gcc asm template argument modifiers
/// to those accepted by the Rust asm! macro. This is arch-dependent, so we need
/// to know which architecture the asm targets.
/// See <https://doc.rust-lang.org/nightly/reference/inline-assembly.html#template-modifiers>
fn translate_modifier(modifier: char, arch: Arch) -> Option<char> {
    Some(match arch {
        Arch::X86 | Arch::X86_64 => match modifier {
            'k' => 'e',
            'q' => 'r',
            'b' => 'l',
            'h' => 'h',
            'w' => 'x',
            _ => return None,
        },
        Arch::Aarch64 => modifier,
        Arch::Arm => match modifier {
            'p' | 'q' => return None,
            _ => modifier,
        },
        Arch::Riscv => match modifier {
            // GCC RISC-V's `%z` modifier means "print the literal `zero`
            // mnemonic if this operand is the constant 0 (typically
            // reached via the 'J' alternative of a multi-letter
            // constraint like 'rJ'/'Jr'), otherwise print the operand's
            // assigned register normally." `parse_constraints` always
            // resolves such multi-letter constraints to the 'r' (any
            // GPR) alternative (see its handling of arch-generic
            // fallback letters) rather than ever emitting the immediate
            // encoding that would make an operand actually become the
            // `zero` register, so by the time a Rust `asm!` operand
            // reaches this point it is unconditionally a real, allocated
            // GPR - the "print `zero`" case can never apply, and the
            // "print the register normally" case is exactly what Rust's
            // `{N}` substitution already does with no modifier at all.
            // Rust's `reg` register class also does not support *any*
            // template modifier (rustc hard-errors on one), so the
            // correct translation is to drop `z` rather than pass it
            // through unchanged.
            'z' => return None,
            _ => modifier,
        },
    })
}

/// Rust-native asm! operands, which may be inputs, outputs, or both.
struct BidirAsmOperand {
    dir_spec: ArgDirSpec,
    mem_only: bool,
    constraints: String,
    name: Option<String>,
    // At least one of these is non-None
    in_expr: Option<(usize, CExprId)>,
    out_expr: Option<(usize, CExprId)>,
}

impl BidirAsmOperand {
    /// Return whether an operand is positional (as opposed to named or using an explicit register)
    fn is_positional(&self) -> bool {
        !self.constraints.contains('"') && self.name.is_none()
    }

    /// Return whether this operand occurs at the given position in the original sequence of
    /// [outputs...inputs]. For tied operands, both input and output index is considered.
    fn has_orig_idx(&self, orig_idx: usize) -> bool {
        match (self.out_expr, self.in_expr) {
            (Some((idx, _)), _) if idx == orig_idx => true,
            (_, Some((idx, _))) if idx == orig_idx => true,
            _ => false,
        }
    }
}

/// Return the register and corresponding template modifiers if the constraint
/// uses a reserved register.
fn reg_is_reserved(constraint: &str, arch: Arch) -> Option<(&str, &str)> {
    Some(match arch {
        Arch::X86 => match constraint {
            // esi is reserved on x86
            "\"esi\"" | "\"si\"" => {
                let reg = constraint.trim_matches('"');
                // "e" if esi, "" if si
                let mods = &reg[..reg.len() - 2];
                (reg, mods)
            }
            _ => return None,
        },
        Arch::X86_64 => match constraint {
            // rbx is reserved on x86_64
            "\"bl\"" | "\"bh\"" | "\"bx\"" | "\"ebx\"" | "\"rbx\"" => {
                let reg = constraint.trim_matches('"');
                let mods = if reg.len() == 2 {
                    &reg[1..] // l/h/x
                } else {
                    &reg[..1] // e/r
                };
                (reg, mods)
            }
            _ => return None,
        },
        _ => return None,
    })
}

/// Emit mov instructions and modify inline assembly operands to copy in and/or
/// out when an operand uses a reserved register. Instead of constraining the
/// operand to the reserved register, constrain it to any register and then copy
/// between the reserved register and the (suitably modified, e.g. `{0:x}`)
/// operand.
/// This also requires reordering the operands because we convert them to
/// named operands, which must precede explicit register operands.
///
/// Modifies `operands` and returns a pair of prefix and suffix strings that
/// should be appended to the assembly template.
fn rewrite_reserved_reg_operands(
    att_syntax: bool,
    arch: Arch,
    operands: &mut [BidirAsmOperand],
) -> (String, String) {
    let (mut prolog, mut epilog) = (String::new(), String::new());

    let mut rewrite_idxs = vec![];
    let mut total_positional = 0;

    // Determine which operands must be rewritten and how many
    // positional operands there are. Positional operands must precede named
    // operands, so this tells us where to reinsert operands we rewrite.
    for (i, operand) in operands.iter().enumerate() {
        if operand.is_positional() {
            total_positional += 1;
        } else if let Some((reg, mods)) = reg_is_reserved(&operand.constraints, arch) {
            rewrite_idxs.push((i, reg.to_owned(), mods.to_owned()));
        }
    }

    for (n_moved, (idx, reg, mods)) in rewrite_idxs.into_iter().enumerate() {
        let operand = &mut operands[idx];
        let name = format!("restmp{}", n_moved);
        if let Some((_idx, _in_expr)) = operand.in_expr {
            let move_input = if att_syntax {
                format!("mov %{}, {{{}:{}}}\n", reg, name, mods)
            } else {
                format!("mov {{{}:{}}}\n, {}", name, mods, reg)
            };
            prolog.push_str(&move_input);
        }
        if let Some((_idx, _out_expr)) = operand.out_expr {
            let move_output = if att_syntax {
                format!("\nmov {{{}:{}}}, %{}", name, mods, reg)
            } else {
                format!("\nmov {}, {{{}:{}}}", reg, name, mods)
            };
            epilog.push_str(&move_output);
        }
        operand.constraints = "reg".into();
        operand.name = Some(name);

        // Move operand to after all positional arguments. This does not
        // interfere with moving subsequent operands that use reserved registers
        // because explicit register operands must all come after positional
        // and named operands.
        //let (positional, named, explicit) = split_operands(operands);
        let nth_non_positional = total_positional + n_moved;
        operands.swap(idx, nth_non_positional);
    }

    (prolog, epilog)
}

/// Remove comments from an x86 assembly template. Used only to provide a less-
/// confounding input to our Intel-vs-AT&T detection in `asm_is_att_syntax`.
fn remove_comments(mut asm: &str) -> String {
    // Remove C-style comments
    let mut without_c_comments = String::with_capacity(asm.len());
    while let Some(comment_begin) = asm.find("/*") {
        let comment_len = asm[comment_begin..]
            .find("*/")
            // Comments with no terminator extend to the end of the string
            .unwrap_or_else(|| asm[comment_begin..].len());
        let before_comment = &asm[..comment_begin];
        without_c_comments.push_str(before_comment);
        asm = &asm[comment_begin + comment_len..];
    }
    // Push whatever is left after the final comment
    without_c_comments.push_str(asm);

    // Remove EOL comments from each line
    let mut without_comments = String::with_capacity(without_c_comments.len());
    for line in without_c_comments.lines() {
        if let Some(line_comment_idx) = line.find('#') {
            without_comments.push_str(&line[..line_comment_idx]);
        } else {
            without_comments.push_str(line);
        }
        without_comments.push('\n');
    }
    without_comments
}

/// Detect whether an x86(_64) extended asm template string uses AT&T syntax (vs. Intel).
/// For gcc, AT&T syntax is default... unless `-masm=intel` is passed. This
/// means we can hope but not guarantee that x86 asm with no syntax directive
/// uses AT&T syntax.
/// To handle other cases, try to heuristically detect the variant we get
/// (assuming it's actually x86 asm in the first place...).
/// As the rust x86 default is intel syntax, we need to emit the "att_syntax"
/// option if we get a hint that this asm uses AT&T syntax.
///
/// Note that this function receives the asm template after Clang's translation
/// from gcc syntax (with `%` for substitution/escapes) to LLVM syntax which
/// uses `$` for substitution/escapes. See:
/// <https://llvm.org/docs/LangRef.html#inline-assembler-expressions>
fn asm_is_att_syntax(asm: &str) -> bool {
    // First, remove comments, so we can look at only the semantically
    // significant parts of the asm template.
    let asm = &*remove_comments(asm);

    // Look for syntax directives.
    let intel_directive = asm.find(".intel_syntax");
    let att_directive = asm.find(".att_syntax");
    match (intel_directive, att_directive) {
        (Some(intel_pos), Some(att_pos)) => {
            // Both directives are present; presumably this asm switches to one at
            // its start and restores the default at the end. Whichever comes first
            // should be what the asm uses.
            att_pos < intel_pos
        }
        (Some(_intel), None) => false,
        (None, Some(_att)) => true,
        (None, None) => {
            #[allow(clippy::needless_bool)]
            if asm.contains("word ptr") {
                false
            } else if asm.contains("$$") || asm.contains('%') || asm.contains('(') {
                // Guess based on sigils used in AT&T assembly:
                // $ (escaped) for constants, % for registers, and ( for address calculations
                true
            } else if asm.contains('[') {
                // default to true, because AT&T is the default for gcc inline asm
                false
            } else {
                true
            }
        }
    }
}

/// If applicable, maps the index of an input operand to the
/// output operand it is tied to.
///
/// Utilizes the fact that GNU inline assembly specifies all
/// operands in a particular order of [output1, ... outputN]
/// followed by [input1, ..., inputN] where the indices for all
/// input operands are indexed relative to the size of the
/// output operand sequence.
fn tied_output_operand_idx(
    idx: usize,
    num_output_operands: usize,
    tied_operands: &HashMap<(usize, bool), usize>,
) -> usize {
    if let Some(adj_idx) = idx.checked_sub(num_output_operands) {
        // will only be Some(idx) if it was an input operand
        match tied_operands.get(&(adj_idx, false)) {
            Some(&out_idx) => {
                return out_idx;
            }
            None => {
                // get number of tied inputs before this index
                // TODO: can calculate this once before the call, not once for each input
                let num_tied_before = tied_operands
                    .keys()
                    .filter(|&&(iidx, is_out)| !is_out && iidx < adj_idx)
                    .count();
                // shift the original index by the number of tied input operands prior to the current one
                return idx - num_tied_before;
            }
        };
    }

    idx
}

/// Scan an (LLVM-syntax, `$`-escaped) asm template for any operand reference
/// (`$N`, `$xN`, or `${N:mod}`) whose index is `>= min_idx`. Used to detect
/// references to GNU asm-goto label operands, which are numbered
/// contiguously after all outputs and inputs (see `GCCAsmStmt::begin_labels`
/// in Clang) but are never extracted by the AST exporter or represented in
/// `AsmOperand`/`CStmtKind::Asm`, so no input/output data exists for them.
fn references_operand_at_or_beyond(asm: &str, min_idx: usize) -> bool {
    let mut first = true;
    let mut last_empty = false;
    for chunk in asm.split('$') {
        if first {
            first = false;
            continue;
        }
        if last_empty {
            last_empty = false;
            continue;
        }
        if chunk.is_empty() {
            last_empty = true;
            continue;
        }

        // ${N:mod} form
        let digits: String = if let Some(rest) = chunk.strip_prefix('{') {
            rest.chars()
                .skip_while(|c| !c.is_ascii_digit())
                .take_while(|c| c.is_ascii_digit())
                .collect()
        } else {
            // $N or $xN form: skip a leading alphabetic modifier, if any
            let rest = if chunk.starts_with(|c: char| c.is_ascii_alphabetic()) {
                &chunk[1..]
            } else {
                chunk
            };
            rest.chars().take_while(|c| c.is_ascii_digit()).collect()
        };

        if let Ok(idx) = digits.parse::<usize>() {
            if idx >= min_idx {
                return true;
            }
        }
    }
    false
}

/// Scan an (LLVM-syntax, `$`-escaped) asm template and collect every
/// operand index it references (`$N`, `$xN`, or `${N:mod}` forms).
///
/// GCC/Clang extended asm tolerates declaring an output operand that no
/// `%N` (here, already-lowered to `$N`) in the template ever writes to -
/// GCC simply never emits code touching that operand, silently treating it
/// as a no-op (seen in the kernel's own `arch/riscv/include/asm/vector.h`
/// `__vstate_csr_save`, which declares 4 outputs but only references 3).
/// Rust's `asm!` has no equivalent tolerance and rejects any operand that
/// isn't referenced by the template outright ("argument never used"). This
/// is used to find such unreferenced output operands so they can be
/// rendered as discarded (`_`) rather than bound to the real output
/// expression - since the instruction sequence never actually writes to
/// that operand, `_` (an arbitrary scratch register, value discarded)
/// preserves the original (odd, but real) semantics of "this operand is
/// declared but never actually written," rather than either failing to
/// compile or, if we invented a fake reference just to satisfy rustc,
/// writing a bogus value into the caller's variable.
fn referenced_operand_indices(asm: &str) -> std::collections::HashSet<usize> {
    let mut indices = std::collections::HashSet::new();
    let mut first = true;
    let mut last_empty = false;
    for chunk in asm.split('$') {
        if first {
            first = false;
            continue;
        }
        if last_empty {
            last_empty = false;
            continue;
        }
        if chunk.is_empty() {
            last_empty = true;
            continue;
        }

        let digits: String = if let Some(rest) = chunk.strip_prefix('{') {
            rest.chars()
                .skip_while(|c| !c.is_ascii_digit())
                .take_while(|c| c.is_ascii_digit())
                .collect()
        } else {
            let rest = if chunk.starts_with(|c: char| c.is_ascii_alphabetic()) {
                &chunk[1..]
            } else {
                chunk
            };
            rest.chars().take_while(|c| c.is_ascii_digit()).collect()
        };

        if let Ok(idx) = digits.parse::<usize>() {
            indices.insert(idx);
        }
    }
    indices
}

/// Rewrite a LLVM inline assembly template string into an asm!-compatible one
/// by translating its references to operands (of the form $0 or $x0) to {0} or
/// {0:y} (and wrapping mem-only references in square brackets).
fn rewrite_asm<F: Fn(&str) -> bool, M: Fn(usize) -> TranslationResult<usize>>(
    asm: &str,
    att_syntax: bool,
    input_op_mapper: M,
    is_mem_only: F,
    arch: Arch,
) -> TranslationResult<String> {
    let mut out = String::with_capacity(asm.len());

    let mut first = true;
    let mut last_empty = false;

    // Iterate over $-prefixed chunks
    for chunk in asm.split('$') {
        // No modification needed for first chunk
        if first {
            first = false;
            out.push_str(chunk);
            continue;
        }

        // Pass-through $$ as one $
        if last_empty {
            last_empty = false;
            out.push('$');
            out.push_str(chunk);
            continue;
        }

        // Note empty chunks
        if chunk.is_empty() {
            last_empty = true;
            continue;
        }

        // Do not re-wrap ${...}, but do translate modifiers
        if chunk.starts_with('{') {
            // Translate operand modifiers ("template modifiers" per Rust)
            if let Some(end_idx) = chunk.find('}') {
                let ref_str = &chunk[..end_idx];
                if let Some(colon_idx) = ref_str.find(':') {
                    let (before_mods, _modifiers) = ref_str.split_at(colon_idx + 1);
                    out.push('{');
                    let idx: usize = before_mods
                        .trim_matches(|c: char| !c.is_ascii_digit())
                        .parse()
                        .map_err(|_| TranslationError::generic("could not parse operand idx"))?;
                    out.push_str(input_op_mapper(idx)?.to_string().as_str());
                    out.push(':');
                    let modifiers = ref_str[colon_idx + 1..].chars();
                    for modifier in modifiers {
                        if let Some(new) = translate_modifier(modifier, arch) {
                            out.push(new);
                        }
                    }
                    out.push_str(&chunk[end_idx..]);
                }
            } else {
                out.push_str(chunk);
            }
            continue;
        }

        // Translate references of the form %k0 or %3, which look like $k0
        // or $3 in LLVM asm.
        if chunk.starts_with(|c: char| c.is_ascii_alphanumeric()) {
            // Find the end of the reference itself (after 'k0' or '3').
            let end_idx = chunk
                .find(|c: char| c == ',' || !c.is_ascii_alphanumeric())
                .unwrap_or(chunk.len());
            let ref_str = &chunk[..end_idx];

            let index_str;
            let mut new_modifiers = String::new();
            // If the ref string starts with a letter, it's a modifier to translate.
            if let Some(true) = ref_str.chars().next().map(|c| c.is_ascii_alphabetic()) {
                let (modifiers, index) = ref_str.split_at(1);

                index_str = index;

                for modifier in modifiers.chars() {
                    if let Some(new) = translate_modifier(modifier, arch) {
                        new_modifiers.push(new);
                    }
                }
            } else {
                // Just digits
                index_str = ref_str;
            }
            let mem_only = is_mem_only(index_str);
            // Push the reference wrapped in {}, or in [{}] if mem-only
            if mem_only {
                out.push(if att_syntax { '(' } else { '[' });
            };
            out.push('{');
            let idx: usize = index_str
                .parse()
                .map_err(|_| TranslationError::generic("could not parse operand idx"))?;
            out.push_str(input_op_mapper(idx)?.to_string().as_str());
            if !new_modifiers.is_empty() {
                out.push(':');
                out.push_str(&new_modifiers);
            }
            out.push('}');
            if mem_only {
                out.push(if att_syntax { ')' } else { ']' });
            };
            // Push the rest of the chunk
            out.push_str(&chunk[end_idx..]);
            continue;
        }

        // We failed to parse this operand reference
        out.push_str(chunk);
    }
    Ok(out)
}

impl<'c> Translation<'c> {
    /// Convert an inline-assembly statement into one or more Rust statements.
    /// If inline assembly translation is not enabled this will result in an
    /// error message instead of a conversion. Because the inline assembly syntax
    /// used in C is different than the one used in Rust (Rust uses the LLVM syntax
    /// directly) the resulting translated assembly statements will be unlikely to work
    /// without further manual translation. The translator will properly translate
    /// the arguments to the assembly statement, however.
    pub fn convert_asm(
        &self,
        ctx: ExprContext,
        span: Span,
        is_volatile: bool,
        asm: &str,
        inputs: &[AsmOperand],
        outputs: &[AsmOperand],
        clobbers: &[String],
    ) -> TranslationResult<Vec<Stmt>> {
        if !self.tcfg.translate_asm {
            return Err(TranslationError::generic(
                "Inline assembly translation not enabled.",
            ));
        }

        let arch = match parse_arch(&self.ast_context.target) {
            Some(arch) => arch,
            None => {
                return Err(TranslationError::generic(
                    "Cannot translate inline assembly for unfamiliar architecture",
                ))
            }
        };

        // GCC/Clang extended asm supports a third operand class beyond
        // outputs/inputs: "label operands" (GNU `asm goto`), referenced in the
        // template via `%l[name]` / `${N:l}` (the `l` template modifier). These
        // bind to C `goto` labels rather than expressions and are numbered
        // contiguously *after* all outputs and inputs (Clang's `GCCAsmStmt`
        // keeps a single `Exprs` array laid out as
        // `[outputs..., inputs..., labels...]`, see `clang/AST/Stmt.h`
        // `GCCAsmStmt::begin_labels()`).
        //
        // Neither the AST exporter (`AstExporter.cpp`'s `VisitGCCAsmStmt`) nor
        // `CStmtKind::Asm`/`AsmOperand` on the Rust side model label operands
        // at all: only `E->begin_inputs()/end_inputs()` and
        // `E->begin_outputs()/end_outputs()` are walked, and only input/output
        // constraint strings are encoded. So an asm template that references
        // operand index `outputs.len() + inputs.len() + k` (a label operand)
        // has no corresponding entry in `inputs`/`outputs` at all - not a bug
        // in the lookup, but a construct we never extract from the Clang AST
        // in the first place.
        //
        // Fully supporting this would mean plumbing a `labels: Vec<..>` field
        // through the exporter and `CStmtKind::Asm`, and emitting Rust's own
        // `asm!` label-operand syntax (`label { .. }`) - itself a genuine
        // control-flow construct, not just another substitution. Given this
        // transpiler is used here as a differential/reference tool (asm output
        // is always hand-reviewed and reimplemented, never trusted as-is), we
        // degrade gracefully instead: reject asm-goto statements up front with
        // a clear error so the containing function is skipped with a warning,
        // rather than translating a template with dangling operand references.
        let num_extractable_operands = outputs.len() + inputs.len();
        if references_operand_at_or_beyond(asm, num_extractable_operands) {
            return Err(TranslationError::generic(
                "Cannot translate GNU asm goto (extended asm with label operands): \
                 label operands are not extracted from the Clang AST and have no \
                 Rust `asm!` equivalent that can be generated automatically.",
            ));
        }

        // Find output operands that the template never actually
        // substitutes (see `referenced_operand_indices`'s doc comment).
        // These get bound to `_` (discarded) below rather than to the
        // real output expression, since GCC's own behavior for these is
        // to silently never write to them.
        let referenced_indices = referenced_operand_indices(asm);
        let unreferenced_outputs: std::collections::HashSet<usize> = (0..outputs.len())
            .filter(|idx| !referenced_indices.contains(idx))
            .collect();

        // `core::arch::asm!` needs no feature gate: stable since Rust 1.59
        // (Feb 2022). See the matching note on `convert_file_scope_asm`.

        fn push_expr(tokens: &mut Vec<TokenTree>, expr: Box<Expr>) {
            tokens.extend(expr.to_token_stream());
        }

        let mut stmts: Vec<Stmt> = vec![];
        let mut post_stmts: Vec<Stmt> = vec![];
        let mut tokens: Vec<TokenTree> = vec![];

        // Identify tied operands
        let mut tied_operands = HashMap::new();
        for (input_idx, AsmOperand { constraints, .. }) in inputs.iter().enumerate() {
            let constraints_digits = constraints.trim_matches(|c: char| !c.is_ascii_digit());
            if let Ok(output_idx) = constraints_digits.parse::<usize>() {
                let output_key = (output_idx, true);
                // Positional references in template count across outputs then inputs.
                // Add output count so that our index is into the full sequence and can be used for
                // positional template substitutions.
                let input_key = (input_idx + outputs.len(), false);
                tied_operands.insert(output_key, input_idx);
                tied_operands.insert(input_key, output_idx);
            }
        }

        let operand_is_mem_only = |operand: &AsmOperand| -> bool {
            if let Ok((_dir_spec, mem_only, _parsed)) =
                parse_constraints(&operand.constraints, arch)
            {
                mem_only
            } else {
                println!("could not parse asm constraints: {}", operand.constraints);
                false
            }
        };

        // Detect and pair inputs/outputs that constrain themselves to the same register
        let mut inputs_by_register = HashMap::new();
        let mut other_inputs = Vec::new();
        for (i, input) in inputs.iter().enumerate() {
            let combined_idx = i + outputs.len();
            let (_dir_spec, _mem_only, parsed) = parse_constraints(&input.constraints, arch)?;
            // Only pair operands with an explicit register or index
            if is_regname_or_int(&parsed) {
                inputs_by_register.insert(parsed, (combined_idx, input.clone()));
            } else {
                other_inputs.push((parsed, (combined_idx, input.clone())));
            }
        }

        // Convert gcc asm arguments (input and output lists) into a single list
        // of operands with explicit arg dir specs (asm!-style)

        // The unified arg list
        let mut args = Vec::new();

        // Add outputs as inout if a matching input is found, else as outputs
        //
        // A constraint that fails to parse (e.g. a machine constraint with
        // no safe Rust asm! translation) used to be silently dropped here,
        // logging to stderr but otherwise proceeding to emit an asm!
        // invocation missing that operand entirely - which either leaves a
        // dangling `{N}` reference in the template (a confusing, unrelated
        // compile error) or, worse, silently shifts every subsequent
        // operand's index, changing which value each `{N}` in the template
        // actually binds to. That silent-corruption risk is exactly what
        // this translator's operand handling must not do, so propagate the
        // error and fail the whole statement instead: the caller degrades
        // this to a translation error for just the containing function,
        // per the project's "no output rather than wrong output" pattern.
        for (output_idx, output) in outputs.iter().enumerate() {
            let (mut dir_spec, mem_only, parsed) = parse_constraints(&output.constraints, arch)?;
            // Add to args list; if a matching in_expr is found, this is
            // an inout and we remove the output from the outputs list
            let mut in_expr = inputs_by_register.remove(&parsed);
            if in_expr.is_none() {
                // Also check for by-index references to this output
                in_expr = inputs_by_register.remove(&output_idx.to_string());
            }
            // Extract expression
            let in_expr = in_expr.map(|(i, operand)| (i, operand.expression));

            // Drop pure-output operands (no tied input) that the asm
            // template never actually references (see
            // `referenced_operand_indices`). An inout/tied operand is never
            // dropped here even if unreferenced under its *output* index,
            // because the corresponding input still needs to be wired in;
            // that case is instead caught by the same
            // `unreferenced_outputs` check re-applied to its (renumbered)
            // combined index further below, via `new_idx_for_orig` still
            // finding this operand - the only thing we can safely skip
            // without losing real dataflow is a standalone, wholly-unused
            // output.
            if in_expr.is_none() && unreferenced_outputs.contains(&output_idx) {
                continue;
            }

            // For inouts, change the dirspec to include 'in'
            if in_expr.is_some() {
                dir_spec = dir_spec.with_in();
            }
            args.push(BidirAsmOperand {
                dir_spec,
                mem_only,
                name: None,
                constraints: parsed,
                in_expr,
                out_expr: Some((output_idx, output.expression)),
            });
        }
        // Add unmatched inputs
        for (_, (input_idx, input)) in inputs_by_register
            .into_iter()
            .chain(other_inputs.into_iter())
        {
            let (dir_spec, mem_only, parsed) = parse_constraints(&input.constraints, arch)?;
            args.push(BidirAsmOperand {
                dir_spec,
                mem_only,
                name: None,
                constraints: parsed,
                in_expr: Some((input_idx, input.expression)),
                out_expr: None,
            });
        }

        // Determine whether the assembly is in AT&T syntax
        let att_syntax = match arch {
            Arch::X86 | Arch::X86_64 => asm_is_att_syntax(asm),
            _ => false,
        };

        // Sort positional args before named ones.
        args.sort_by_key(|arg| !arg.is_positional());

        // Add workaround for reserved registers (e.g. rbx on x86_64)
        let (prolog, epilog) = rewrite_reserved_reg_operands(att_syntax, arch, &mut args);

        // Find new idx by searching args for one with this original idx. This
        // can legitimately fail to find a match for asm templates that
        // reference operand indices we don't model at all (e.g. GNU asm-goto
        // label operands; see the `references_operand_at_or_beyond` check
        // above, which should already reject those before we get here). Treat
        // any such miss as a translation error rather than panicking, so an
        // unanticipated gap here degrades the containing function instead of
        // aborting the whole transpile run.
        let new_idx_for_orig = |orig_idx| -> TranslationResult<usize> {
            args.iter()
                .position(|operand| operand.has_orig_idx(orig_idx))
                .ok_or_else(|| {
                    TranslationError::new(
                        None,
                        failure::err_msg(format!(
                            "no operand had index {orig_idx} in asm str:\n{asm}"
                        ))
                        .context(TranslationErrorKind::Generic),
                    )
                })
        };

        // Rewrite arg references in assembly template
        let rewritten_asm = rewrite_asm(
            asm,
            att_syntax,
            |idx: usize| {
                new_idx_for_orig(tied_output_operand_idx(idx, outputs.len(), &tied_operands))
            },
            |ref_str: &str| {
                if let Ok(idx) = ref_str.parse::<usize>() {
                    outputs
                        .iter()
                        .chain(inputs.iter())
                        .nth(idx)
                        .map(operand_is_mem_only)
                        .unwrap_or(false)
                } else {
                    false
                }
            },
            arch,
        )?;

        let mut rewritten_asm = prolog + &rewritten_asm + &epilog;

        // Some surviving operands are never referenced by the template at
        // all, yet must still be kept (unlike the pure-output case dropped
        // above) because they carry real dataflow - most commonly a tied
        // inout compiler-barrier idiom with an empty asm string, e.g.
        // `include/linux/compiler.h`'s `OPTIMIZER_HIDE_VAR`:
        // `asm("" : "=r"(var) : "0"(var))`, which relies on the compiler
        // being *unable* to reason about what happens to `var` even though
        // no real instruction touches it. Rust's asm! requires every
        // operand to appear in the template at least once regardless of
        // its direction ("argument never used" otherwise), so append a
        // trailing asm-comment line referencing each such operand's index
        // - exactly the workaround rustc's own diagnostic suggests
        // ("consider using it in an asm comment"). Comments are stripped
        // before assembly, so this cannot change the emitted instructions;
        // it only keeps the operand "live" for Rust's own bookkeeping,
        // which is exactly what the original GCC/Clang asm relied on too.
        let mut has_comment_only_operand = false;
        for (new_idx, operand) in args.iter().enumerate() {
            let referenced = operand
                .out_expr
                .map(|(idx, _)| referenced_indices.contains(&idx))
                .unwrap_or(false)
                || operand
                    .in_expr
                    .map(|(idx, _)| referenced_indices.contains(&idx))
                    .unwrap_or(false);
            if !referenced {
                rewritten_asm.push_str(&format!("/* {{{new_idx}}} */\n"));
                has_comment_only_operand = true;
            }
        }

        // Emit assembly template
        for line in rewritten_asm.split('\n') {
            push_expr(&mut tokens, mk().lit_expr(line.to_string() + "\n"));
            tokens.push(TokenTree::Punct(Punct::new(',', Alone)));
        }
        tokens.pop();

        // Whether any operand actually being emitted writes to something
        // (`out`/`lateout`/`inout`/`inlateout`). Rust's `asm!` requires the
        // `pure` option to have at least one real output. Counting on
        // `outputs.len()` (the raw GNU AST output count) instead is not
        // equivalent: a `+`/mem-only-constrained operand can resolve to a
        // plain `In` direction (see `parse_constraints`'s mode selection),
        // and an unreferenced pure-output operand may have just been
        // dropped from `args` entirely above - both cases leave `args`
        // with zero real outputs despite `outputs.len() > 0`, which used
        // to let `pure` be emitted on an asm! with no actual output,
        // hard-rejected by rustc (`asm with the 'pure' option must have at
        // least one output`).
        let has_real_output = args
            .iter()
            .any(|op| !matches!(op.dir_spec, ArgDirSpec::In));

        // Outputs and Inputs
        let mut operand_renames = HashMap::new();
        for operand in args {
            tokens.push(TokenTree::Punct(Punct::new(',', Alone)));

            // First, convert output expr if present
            let out_expr = if let Some((output_idx, out_expr)) = operand.out_expr {
                let mut out_expr = self.convert_expr(ctx.used(), out_expr, None)?;
                stmts.append(out_expr.stmts_mut());
                let mut out_expr = out_expr.into_value();

                if operand.mem_only {
                    // If the constraint string contains `*`, then
                    // c2rust-ast-exporter added it (there's no gcc equivalent);
                    // in this case, we need to do what clang does and pass in
                    // the operand by-address instead of by-value
                    self.use_feature("raw_ref_op");
                    out_expr = mk().mutbl().raw_borrow_expr(out_expr);
                }

                if let Some(_tied_operand) = tied_operands.get(&(output_idx, true)) {
                    // If we have an input operand tied to an output operand,
                    // we need to replicate clang's behavior: the inline assembly
                    // uses the larger type internally, and the smaller value gets
                    // extended to the larger one before the call, and truncated
                    // back after (if needed). For portability, we moved the
                    // type conversions into the `c2rust-asm-casts` crate,
                    // so we call into that one from here.

                    // Convert `x` into `let c2rust_output = &raw mut x; *x`
                    self.use_feature("raw_ref_op");
                    let output_name = self.renamer.borrow_mut().pick_name("c2rust_output");
                    let output_local = mk().local(
                        mk().ident_pat(&output_name),
                        None,
                        Some(mk().mutbl().raw_borrow_expr(out_expr)),
                    );
                    stmts.push(mk().local_stmt(Box::new(output_local)));

                    // `let mut c2rust_inner;`
                    let inner_name = self.renamer.borrow_mut().pick_name("c2rust_inner");
                    let inner_local = mk().local(mk().ident_pat(&inner_name), None, None);
                    stmts.push(mk().local_stmt(Box::new(inner_local)));

                    out_expr = mk().ident_expr(&inner_name);
                    operand_renames.insert(output_idx, (output_name, inner_name));
                }
                Some(out_expr)
            } else {
                None
            };

            // Then, handle input expr if present
            let in_expr = if let Some((input_idx, in_expr)) = operand.in_expr {
                let mut in_expr = self.convert_expr(ctx.used(), in_expr, None)?;
                stmts.append(in_expr.stmts_mut());
                let mut in_expr = in_expr.into_value();

                if operand.mem_only {
                    self.use_feature("raw_ref_op");
                    in_expr = mk().raw_borrow_expr(in_expr);
                }
                if let Some(tied_operand) = tied_operands.get(&(input_idx, false)) {
                    self.use_crate(ExternCrate::C2RustAsmCasts);

                    // Import the trait into scope
                    self.with_cur_file_item_store(|item_store| {
                        item_store.add_use(true, vec!["c2rust_asm_casts".into()], "AsmCastTrait");
                    });

                    let (output_name, inner_name) = operand_renames.get(tied_operand).unwrap();

                    let input_name = self.renamer.borrow_mut().pick_name("c2rust_input");
                    let input_local = mk().local(mk().ident_pat(&input_name), None, Some(in_expr));
                    stmts.push(mk().local_stmt(Box::new(input_local)));

                    // Replace `in_expr` with
                    // `c2rust_asm_casts::AsmCast::cast_in(output, input)`
                    let path_expr = mk().path_expr(vec!["c2rust_asm_casts", "AsmCast", "cast_in"]);
                    let output = mk().ident_expr(output_name);
                    let input = mk().ident_expr(input_name);
                    in_expr = mk().call_expr(path_expr, vec![output.clone(), input.clone()]);

                    // Append the cast-out call after the assembly macro:
                    // `c2rust_asm_casts::AsmCast::cast_out(output, input, inner);`
                    let path_expr = mk().path_expr(vec!["c2rust_asm_casts", "AsmCast", "cast_out"]);
                    let inner = mk().ident_expr(inner_name);
                    let cast_out = mk().call_expr(path_expr, vec![output, input, inner]);
                    post_stmts.push(mk().semi_stmt(cast_out));
                }
                Some(in_expr)
            } else {
                None
            };

            // Emit "name =" if a name is given
            if let Some(name) = operand.name {
                push_expr(&mut tokens, mk().ident_expr(name));
                tokens.push(TokenTree::Punct(Punct::new('=', Alone)));
            }

            // Emit dir_spec(constraint), quoting constraint if needed
            push_expr(&mut tokens, mk().ident_expr(operand.dir_spec.to_string()));
            let constraints_ident = if is_regname_or_int(&operand.constraints) {
                mk().lit_expr(operand.constraints.trim_matches('"'))
            } else {
                mk().ident_expr(operand.constraints)
            };

            // Emit input and/or output expressions, separated by "=>" if both
            push_expr(&mut tokens, mk().paren_expr(constraints_ident));
            if let Some(in_expr) = in_expr {
                let in_expr_span = in_expr.span();
                push_expr(&mut tokens, in_expr);
                if out_expr.is_some() {
                    tokens.push(TokenTree::Punct(Punct::new('=', Joint)));
                    tokens.push(TokenTree::Punct(Punct::new('>', Alone)));
                } else {
                    // If inout but no out expr was given, mark clobbered ('_')
                    if let ArgDirSpec::InOut | ArgDirSpec::InLateOut = operand.dir_spec {
                        tokens.push(TokenTree::Punct(Punct::new('=', Joint)));
                        tokens.push(TokenTree::Punct(Punct::new('>', Alone)));

                        tokens.push(TokenTree::Ident(Ident::new("_", in_expr_span)));
                    }
                }
            }
            if let Some(out_expr) = out_expr {
                push_expr(&mut tokens, out_expr);
            }
        }

        let mut preserves_flags = true;
        let mut read_only = true;
        // Counts only clobbers that actually emit an `out(reg) _` operand
        // below (unlike `clobbers.len()`, which also counts "cc"/"memory"
        // pseudo-clobbers and reserved-register clobbers that get dropped
        // without emitting anything) - this is what actually determines
        // whether there's a real output operand for the `pure` option.
        let mut emitted_register_clobbers = 0usize;

        // Clobbers
        for clobber in clobbers {
            // Process and drop non-register clobbers
            if clobber == "cc" {
                preserves_flags = false;
                continue;
            };
            if clobber == "memory" {
                read_only = false;
                continue;
            };

            // We must drop clobbers of reserved registers, even though this
            // really means we're misinforming the compiler of what's been
            // overwritten. Warn verbosely.
            let quoted = format!("\"{}\"", clobber);
            if reg_is_reserved(&quoted, arch).is_some() {
                warn!(
                    "Attempting to clobber reserved register ({}), dropping clobber! \
                This likely means the potential for miscompilation has been introduced. \
                Please rewrite this assembly to save/restore the value of this register \
                if at all possible.",
                    clobber
                );
                continue;
            }

            tokens.push(TokenTree::Punct(Punct::new(',', Alone)));
            let result = mk().call_expr(mk().ident_expr("out"), vec![mk().lit_expr(clobber)]);
            push_expr(&mut tokens, result);
            push_expr(&mut tokens, mk().ident_expr("_"));
            emitted_register_clobbers += 1;
        }

        // Options
        {
            let mut options = vec![];
            if preserves_flags {
                options.push(mk().ident_expr("preserves_flags"));
            }
            if !is_volatile {
                // Pure cannot be applied if we have no outputs. Register
                // clobbers are emitted as their own `out(reg) _` operands
                // above (not part of `args`/`has_real_output`), so they
                // also count as a real output for this purpose.
                //
                // A statement with a comment-only operand (see
                // `has_comment_only_operand` above) is excluded outright,
                // regardless of read_only/output count: these are
                // compiler-barrier idioms (e.g. OPTIMIZER_HIDE_VAR, used by
                // nospec.h's array_index_mask_nospec for Spectre-v1
                // mitigation) whose entire purpose is to stop the compiler
                // reasoning about the operand's dataflow. `pure` explicitly
                // licenses the compiler to assume outputs are a function of
                // inputs and reorder/CSE/eliminate accordingly - exactly
                // the assumption these barriers exist to block. Applying it
                // here wouldn't fail to compile; it would silently defeat
                // the barrier, which for a speculation-mitigation call site
                // is a security-relevant correctness bug, not just a
                // performance quirk.
                if read_only
                    && !has_comment_only_operand
                    && (has_real_output || emitted_register_clobbers > 0)
                {
                    options.push(mk().ident_expr("pure"));
                    options.push(mk().ident_expr("readonly"));
                }
                // We never emit [pure, nomem] right now, but it would be nice
            }

            if att_syntax {
                options.push(mk().ident_expr("att_syntax"));
            }

            if !options.is_empty() {
                tokens.push(TokenTree::Punct(Punct::new(',', Alone)));
                let result = mk().call_expr(mk().ident_expr("options"), options);
                push_expr(&mut tokens, result);
            }
        }

        self.with_cur_file_item_store(|item_store| {
            item_store.add_use(true, vec!["core".into(), "arch".into()], "asm");
        });

        let mac = mk().mac(
            mk().path(vec!["asm"]),
            tokens.into_iter().collect::<TokenStream>(),
            MacroDelimiter::Paren(Default::default()),
        );
        let mac = mk().mac_expr(mac);
        let mac = mk().span(span).semi_stmt(mac);
        stmts.push(mac);

        // Push the post-macro statements
        stmts.extend(post_stmts);

        Ok(stmts)
    }
}
