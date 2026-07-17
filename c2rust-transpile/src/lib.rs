#![feature(is_some_and)]
#![allow(clippy::too_many_arguments)]

mod diagnostics;

pub mod build_files;
pub mod c_ast;
pub mod cfg;
mod compile_cmds;
pub mod convert_type;
pub mod kernel_idioms;
pub mod renamer;
pub mod rust_ast;
pub mod translator;
pub mod with_stmts;

use std::collections::HashSet;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::{env, io};

use crate::compile_cmds::CompileCmd;
use crate::renamer::RUST_KEYWORDS;
use c2rust_rust_tools::{rustfmt, RustEdition};
use failure::{format_err, Error};
use itertools::Itertools;
use log::{info, warn};
use regex::Regex;
use serde_derive::Serialize;
pub use tempfile::TempDir;
use which::which;

use crate::c_ast::Printer;
use crate::c_ast::*;
pub use crate::diagnostics::{CapturedRecord, Diagnostic};
pub use crate::kernel_idioms::{all_named_rules, parse_rule_list, KernelIdiomRule, KernelIdiomRules};
use c2rust_ast_exporter as ast_exporter;

use crate::build_files::{emit_build_files, get_build_dir, CrateConfig};
use crate::compile_cmds::get_compile_commands;
pub use crate::translator::ReplaceMode;
use std::prelude::v1::Vec;

type PragmaVec = Vec<(&'static str, Vec<&'static str>)>;
type PragmaSet = indexmap::IndexSet<(&'static str, &'static str)>;
type CrateSet = indexmap::IndexSet<ExternCrate>;
type TranspileResult = Result<(PathBuf, PragmaVec, CrateSet), ()>;

#[derive(Default, Debug)]
pub enum TranslateMacros {
    /// Don't translate any macros.
    None,

    /// Translate the conservative subset of macros known to always work.
    #[default]
    Conservative,

    /// Try to translate more, but this is experimental and not guaranteed to work.
    ///
    /// For const-like macros, this works in some cases.
    /// For function-like macros, this doesn't really work at all yet.
    Experimental,
}

/// Configuration settings for the translation process
#[derive(Debug)]
pub struct TranspilerConfig {
    // Debug output options
    pub dump_untyped_context: bool,
    pub dump_typed_context: bool,
    pub pretty_typed_context: bool,
    pub dump_function_cfgs: bool,
    pub json_function_cfgs: bool,
    pub dump_cfg_liveness: bool,
    pub dump_structures: bool,
    pub verbose: bool,
    pub debug_ast_exporter: bool,
    pub emit_c_decl_map: bool,

    // Options that control translation
    pub incremental_relooper: bool,
    pub fail_on_multiple: bool,
    pub filter: Option<Regex>,
    pub debug_relooper_labels: bool,
    pub cross_checks: bool,
    pub cross_check_backend: String,
    pub cross_check_configs: Vec<String>,
    pub prefix_function_names: Option<String>,
    pub translate_asm: bool,
    pub use_c_loop_info: bool,
    pub use_c_multiple_info: bool,
    pub simplify_structures: bool,
    pub panic_on_translator_failure: bool,
    pub emit_modules: bool,
    pub fail_on_error: bool,
    pub replace_unsupported_decls: ReplaceMode,
    pub translate_valist: bool,
    pub overwrite_existing: bool,
    pub reduce_type_annotations: bool,
    pub reorganize_definitions: bool,
    pub enabled_warnings: HashSet<Diagnostic>,
    pub emit_no_std: bool,
    pub output_dir: Option<PathBuf>,
    pub translate_const_macros: TranslateMacros,
    pub translate_fn_macros: TranslateMacros,
    pub disable_rustfmt: bool,
    pub disable_refactoring: bool,
    pub preserve_unused_functions: bool,
    pub log_level: log::LevelFilter,
    pub edition: RustEdition,
    pub deny_unsafe_op_in_unsafe_fn: bool,

    /// Kernel-source-idiom rewrites (see [`kernel_idioms`]) to apply during
    /// translation. Empty by default — see that module's doc comment for
    /// why an unrequested rewrite must never fire.
    pub kernel_idiom_rules: KernelIdiomRules,

    /// Run `c2rust-postprocess` after transpiling and potentially refactoring.
    pub postprocess: bool,

    // Options that control build files
    /// Emit `Cargo.toml` and `lib.rs`
    pub emit_build_files: bool,

    /// Names of translation units containing main functions that we should make
    /// into binaries
    pub binaries: Vec<String>,
    pub thin_binaries: bool,

    pub c2rust_dir: Option<PathBuf>,
}

impl TranspilerConfig {
    fn binary_name_from_path(file: &Path) -> String {
        let file = Path::new(file.file_stem().unwrap());
        get_module_name(file, false, false, false).unwrap()
    }

    fn is_thin_or_full_binary(&self, file: &Path) -> bool {
        let module_name = Self::binary_name_from_path(file);
        self.binaries.contains(&module_name)
    }

    fn is_binary(&self, file: &Path) -> bool {
        // When `--thin-binaries` is enabled, we add all translation
        // units to the main library and emit thin wrappers separately.
        !self.thin_binaries && self.is_thin_or_full_binary(file)
    }

    fn check_if_all_binaries_used(
        &self,
        transpiled_modules: impl IntoIterator<Item = impl AsRef<Path>>,
    ) -> bool {
        let module_names = transpiled_modules
            .into_iter()
            .map(|module| Self::binary_name_from_path(module.as_ref()))
            .collect::<HashSet<_>>();
        let mut ok = true;
        for binary in &self.binaries {
            if !module_names.contains(binary) {
                ok = false;
                warn!("binary not used: {binary}");
            }
        }
        if !ok {
            let module_names = module_names.iter().format(", ");
            info!("candidate modules for binaries are: {module_names}");
        }
        ok
    }

    fn crate_name(&self) -> String {
        self.output_dir
            .as_ref()
            .and_then(|dir| dir.file_name())
            .map(|fname| str_to_ident_checked(fname.to_string_lossy().as_ref(), true))
            .unwrap_or_else(|| "c2rust_out".into())
    }
}

#[derive(Copy, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ExternCrate {
    C2RustBitfields,
    C2RustAsmCasts,
    F128,
    NumTraits,
    Memoffset,
    Libc,
}

#[derive(Serialize)]
struct ExternCrateDetails {
    name: &'static str,
    ident: String,
    macro_use: bool,
    version: &'static str,
    path: Option<PathBuf>,
}

impl ExternCrateDetails {
    pub fn new(
        name: &'static str,
        version: &'static str,
        macro_use: bool,
        path: Option<PathBuf>,
    ) -> Self {
        Self {
            name,
            ident: name.replace('-', "_"),
            macro_use,
            version,
            path,
        }
    }

    /// An external (to c2rust) dependency.
    pub fn external(name: &'static str, version: &'static str, macro_use: bool) -> Self {
        Self::new(name, version, macro_use, None)
    }

    /// An internal (to c2rust) dependency.
    pub fn internal(name: &'static str, macro_use: bool, c2rust_dir: Option<&Path>) -> Self {
        Self::new(
            name,
            env!("CARGO_PKG_VERSION"),
            macro_use,
            c2rust_dir.map(|dir| dir.join(name)),
        )
    }
}

impl ExternCrate {
    fn with_details(&self, c2rust_dir: Option<&Path>) -> ExternCrateDetails {
        use ExternCrate::*;
        match self {
            C2RustBitfields => ExternCrateDetails::internal("c2rust-bitfields", true, c2rust_dir),
            C2RustAsmCasts => ExternCrateDetails::internal("c2rust-asm-casts", true, c2rust_dir),
            F128 => ExternCrateDetails::external("f128", "0.2", false),
            NumTraits => ExternCrateDetails::external("num-traits", "0.2", true),
            Memoffset => ExternCrateDetails::external("memoffset", "0.5", true),
            Libc => ExternCrateDetails::external("libc", "0.2", false),
        }
    }
}

fn char_to_ident(c: char) -> char {
    if c.is_alphanumeric() {
        c
    } else {
        '_'
    }
}

fn str_to_ident(s: &str) -> String {
    s.chars().map(char_to_ident).collect()
}

/// Make sure that name:
/// - does not contain illegal characters,
/// - does not clash with reserved keywords.
fn str_to_ident_checked(s: &str, check_reserved: bool) -> String {
    let s = str_to_ident(s);

    // make sure the name does not clash with keywords
    if check_reserved && RUST_KEYWORDS.contains(&s.as_str()) {
        format!("r#{}", s)
    } else {
        s
    }
}

fn get_module_name(
    file: &Path,
    check_reserved: bool,
    keep_extension: bool,
    full_path: bool,
) -> Option<String> {
    let is_rs = file.extension().map(|ext| ext == "rs").unwrap_or(false);
    let fname = if is_rs {
        file.file_stem()
    } else {
        file.file_name()
    };
    let fname = fname.unwrap().to_str().unwrap();
    let mut name = str_to_ident_checked(fname, check_reserved);
    if keep_extension && is_rs {
        name.push_str(".rs");
    }
    let file = if full_path {
        file.with_file_name(name)
    } else {
        Path::new(&name).to_path_buf()
    };
    file.to_str().map(String::from)
}

pub fn create_temp_compile_commands(sources: &[PathBuf]) -> (TempDir, PathBuf) {
    // If we generate the same path here on every run, then we can't run
    // multiple transpiles in parallel, so we need a unique path. But clang
    // won't read this file unless it is named exactly "compile_commands.json",
    // so we can't change the filename. Instead, create a temporary directory
    // with a unique name, and put the file there.
    let temp_dir = tempfile::Builder::new()
        .prefix("c2rust-")
        .tempdir()
        .expect("Failed to create temporary directory for compile_commands.json");
    let temp_path = temp_dir.path().join("compile_commands.json");

    let compile_commands: Vec<CompileCmd> = sources
        .iter()
        .map(|source_file| {
            let absolute_path = fs::canonicalize(source_file)
                .unwrap_or_else(|_| panic!("Could not canonicalize {}", source_file.display()));

            CompileCmd {
                directory: PathBuf::from("."),
                file: absolute_path.clone(),
                arguments: vec![
                    "clang".to_string(),
                    absolute_path.to_str().unwrap().to_owned(),
                ],
                command: None,
                output: None,
            }
        })
        .collect();

    let json_content = serde_json::to_string(&compile_commands).unwrap();
    let mut file =
        File::create(&temp_path).expect("Failed to create temporary compile_commands.json");
    file.write_all(json_content.as_bytes())
        .expect("Failed to write to temporary compile_commands.json");
    (temp_dir, temp_path)
}

/// Main entry point to transpiler. Called from CLI tools with the result of
/// clap::App::get_matches().
pub fn transpile(tcfg: TranspilerConfig, cc_db: &Path, extra_clang_args: &[&str]) {
    diagnostics::init(tcfg.enabled_warnings.clone(), tcfg.log_level);

    let build_dir = get_build_dir(&tcfg, cc_db);

    let lcmds = get_compile_commands(cc_db, &tcfg.filter).unwrap_or_else(|_| {
        panic!(
            "Could not parse compile commands from {}",
            cc_db.to_string_lossy()
        )
    });

    // Specify path to system include dir on macOS 10.14 and later. Disable the blocks extension.
    let clang_args: Vec<String> = get_extra_args_macos();
    let mut clang_args: Vec<&str> = clang_args.iter().map(AsRef::as_ref).collect();
    clang_args.extend_from_slice(extra_clang_args);

    let mut top_level_ccfg = None;
    let mut workspace_members = vec![];
    let mut num_transpiled_files = 0;
    let mut transpiled_modules = Vec::new();

    for lcmd in &lcmds {
        let cmds = &lcmd.cmd_inputs;
        let lcmd_name = lcmd
            .output
            .as_ref()
            .map(|output| {
                let output_path = Path::new(output);
                output_path
                    .file_stem()
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .to_owned()
            })
            .unwrap_or_else(|| tcfg.crate_name());
        let build_dir = if lcmd.top_level {
            build_dir.to_path_buf()
        } else {
            build_dir.join(&lcmd_name)
        };

        // Compute the common ancestor of all input files
        // FIXME: this is quadratic-time in the length of the ancestor path
        let mut ancestor_path = cmds
            .first()
            .map(|cmd| {
                let mut dir = cmd.abs_file();
                dir.pop(); // discard the file part
                dir
            })
            .unwrap_or_else(PathBuf::new);
        if cmds.len() > 1 {
            for cmd in &cmds[1..] {
                let cmd_path = cmd.abs_file();
                ancestor_path = ancestor_path
                    .ancestors()
                    .find(|a| cmd_path.starts_with(a))
                    .map(ToOwned::to_owned)
                    .unwrap_or_else(PathBuf::new);
            }
        }

        let results = cmds
            .iter()
            .map(|cmd| {
                transpile_single(
                    &tcfg,
                    &cmd.abs_file(),
                    &ancestor_path,
                    &build_dir,
                    cc_db,
                    &clang_args,
                    None,
                )
            })
            .collect::<Vec<TranspileResult>>();
        let mut modules = vec![];
        let mut modules_skipped = false;
        let mut pragmas = PragmaSet::new();
        let mut crates = CrateSet::new();
        for res in results {
            match res {
                Ok((module, pragma_vec, crate_set)) => {
                    modules.push(module);
                    crates.extend(crate_set);

                    num_transpiled_files += 1;
                    for (key, vals) in pragma_vec {
                        for val in vals {
                            pragmas.insert((key, val));
                        }
                    }
                }
                Err(_) => {
                    modules_skipped = true;
                }
            }
        }
        pragmas.sort();
        crates.sort();

        transpiled_modules.extend(modules.iter().cloned());

        if tcfg.emit_build_files {
            if modules_skipped {
                // If we skipped a file, we may not have collected all required pragmas
                warn!("Can't emit build files after incremental transpiler run; skipped.");
                return;
            }

            let ccfg = CrateConfig {
                crate_name: lcmd_name.clone(),
                modules,
                pragmas,
                crates,
                link_cmd: lcmd,
            };
            if lcmd.top_level {
                top_level_ccfg = Some(ccfg);
            } else {
                let crate_file = emit_build_files(&tcfg, &build_dir, Some(ccfg), None);
                let crate_file = crate_file.as_deref();
                reorganize_definitions(&tcfg, &build_dir, crate_file)
                    .unwrap_or_else(|e| warn!("Reorganizing definitions failed: {}", e));
                run_postprocess(&tcfg, &build_dir, crate_file).unwrap_or_else(|e| warn!("{e}"));
                workspace_members.push(lcmd_name);
            }
        }
    }

    if num_transpiled_files == 0 {
        warn!("No C files found in compile_commands.json; nothing to do.");
        return;
    }

    if tcfg.emit_build_files {
        let crate_file =
            emit_build_files(&tcfg, &build_dir, top_level_ccfg, Some(workspace_members));
        let crate_file = crate_file.as_deref();
        reorganize_definitions(&tcfg, &build_dir, crate_file)
            .unwrap_or_else(|e| warn!("Reorganizing definitions failed: {}", e));
        run_postprocess(&tcfg, &build_dir, crate_file).unwrap_or_else(|e| warn!("{e}"));
    }

    tcfg.check_if_all_binaries_used(&transpiled_modules);
}

/// Per-file outcome from [`transpile_batch_with_results`]: everything a
/// caller needs to classify this one TU's transpile result without
/// scraping stderr text, since in-process batching means there's no
/// per-file subprocess stderr to scrape in the first place (see that
/// function's doc comment).
#[derive(Debug, Serialize)]
pub struct FileTranspileResult {
    /// The input path exactly as given in the `compile_commands.json`
    /// entry (not resolved/canonicalized), so a caller can match this back
    /// to the entry it submitted without re-deriving `abs_file()`.
    pub file: PathBuf,
    /// `true` if `output_path.exists() && !tcfg.overwrite_existing`
    /// (`transpile_single`'s existing-file skip) or the input file itself
    /// didn't exist — i.e. `transpile_single` returned `Err(())` before
    /// ever attempting AST export, as opposed to failing during it.
    pub ok: bool,
    /// `Some(message)` if `transpile_single` unwound via `panic!` instead
    /// of returning — the panic hook's formatted message (`'<msg>',
    /// <file>:<line>:<col>`), captured via `diagnostics::
    /// take_last_panic_message` rather than downcasting `catch_unwind`'s
    /// `Box<dyn Any + Send>` payload directly: real in-corpus panics (a
    /// `.expect("Expected decl name")` in translator/mod.rs, an
    /// `indexmap` internal `.expect(...)`) were found via direct testing
    /// to not reliably downcast to `&str`/`String` from within this call
    /// chain on the pinned nightly-2022-11-03 toolchain, even though an
    /// identical `.expect("...")` pattern downcasts fine as a standalone
    /// unit test in this same crate — the hook-captured formatted string
    /// sidesteps that payload-typing question entirely and is the same
    /// text a human reading stderr, or `run_c2rust_baseline.py`'s
    /// `PANIC_RE`, already relies on. `ok` is always `false` when this is
    /// `Some`, since a caught panic and a normal `Err(())` are mutually
    /// exclusive outcomes of the same `catch_unwind` call.
    pub panic_message: Option<String>,
    /// Every `log`-crate record emitted by this file's `transpile_single`
    /// call — covers `diag!`/`log::warn!` sites inside the Rust-side
    /// translator/conversion code (e.g. "Missing top-level node",
    /// "Missing child N of node ..."), as structured `(level, target,
    /// message)` data instead of parsed stderr text. Does NOT cover
    /// diagnostics Clang's C++ `DiagnosticsEngine` emits directly during
    /// AST export (e.g. "Cannot translate GNU address of label
    /// expression") — those never go through Rust's `log` crate at all;
    /// they're in `captured_stderr` instead, alongside these same
    /// records re-rendered as text (fern's formatter also chains to
    /// stderr — see `diagnostics::init` — so anything in `log_records`
    /// appears in `captured_stderr` too, just pre-parsed here for
    /// convenience).
    pub log_records: Vec<CapturedLogRecord>,
    /// Raw fd-level stderr captured for this file only — see
    /// `diagnostics::capture_stderr`'s doc comment for why this exists
    /// (Clang's own diagnostics bypass the `log` crate entirely). This is
    /// the direct structured-batching equivalent of what
    /// `run_c2rust_baseline.py`'s `extract_signatures()` already parses
    /// out of one isolated subprocess's stderr today — a caller
    /// migrating from the per-process path to this one can run the exact
    /// same parsing function against this field unchanged.
    pub captured_stderr: String,
    /// AST-export vs. translation wall-clock split for this file — see
    /// [`PhaseTimings`]. `total_s` is always populated (timed by the
    /// batch loop around the whole `catch_unwind` call, not just by
    /// `transpile_single` itself), but `ast_export_s`/`translate_s` stay
    /// at 0 if a panic unwound before `transpile_single` reached the
    /// point where it writes them back — `panic_message.is_some()` is the
    /// reliable way to tell "genuinely instant" from "phase split
    /// unavailable" apart for those two fields specifically.
    pub phase_timings: PhaseTimings,
}

/// [`CapturedRecord`] with `level` rendered as its `Display` string
/// (`"warning"`/`"error"`/...) instead of the `log::Level` enum, so this
/// type round-trips through `serde_json` without pulling in a `log`
/// dependency on the reading (Python) side.
#[derive(Debug, Serialize)]
pub struct CapturedLogRecord {
    pub level: String,
    pub target: String,
    pub message: String,
}

impl From<CapturedRecord> for CapturedLogRecord {
    fn from(r: CapturedRecord) -> Self {
        Self {
            level: r.level.to_string(),
            target: r.target,
            message: r.message,
        }
    }
}

/// Batched sibling of [`transpile`]: parses one `compile_commands.json`
/// containing MULTIPLE translation units and transpiles every one of them
/// in-process, sequentially, on the calling thread — reusing the exact
/// same `cmds.iter().map(transpile_single)` loop shape `transpile` already
/// runs at line ~397, just with two additions needed to make running N>1
/// files through one process *usable* for a caller that today spawns one
/// OS process per file specifically for crash isolation and per-file
/// diagnostics:
///
/// 1. Each `transpile_single` call is wrapped in `catch_unwind` so a
///    genuine Rust `panic!` in one TU (e.g. the `'Expected decl name'` /
///    `TagTypeUnknown` panics seen against real kernel sources) is
///    contained to that TU's result instead of aborting every remaining
///    file in the batch — the isolation `run_c2rust_baseline.py` gets
///    today from one-process-per-file, reproduced without the process
///    boundary. `ast_exporter::CLANG_MUTEX` is not at risk of poisoning
///    here: it's released (see `get_ast_cbors`) before any Rust-side code
///    that could panic runs, so a caught panic never leaves the mutex
///    locked for the next file in the batch.
/// 2. Log output is captured per-file via `diagnostics::start_capture`/
///    `take_captured` rather than left to free-run to stderr, because
///    with N files sharing one process's stderr stream there is no
///    reliable way for a caller to attribute a given warning line back to
///    the file that produced it (today's one-subprocess-per-file harness
///    gets that attribution for free, from process boundaries). Capturing
///    via the `log` facade directly (structured `Record`s) rather than
///    parsing formatted stderr text is also strictly more precise than
///    what the isolated-process path can extract today: no regex, no risk
///    of two adjacent warnings' "note: expanded from macro" continuation
///    lines being misattributed.
///
/// Deliberately NOT parallelized across a thread pool: `ast_exporter`'s
/// `CLANG_MUTEX` already serializes all `ast_exporter()` FFI calls
/// process-wide (LibTooling/Clang's `CompilerInstance` is not reentrant),
/// so N threads calling this in one process would just contend on that
/// mutex for the (dominant) AST-export phase while adding thread-safety
/// obligations (the `AssertUnwindSafe` below is sound for "one call
/// finishes before the next starts on the same thread"; concurrent calls
/// sharing `tcfg`/`clang_args` would need real auditing, not just a
/// wrapper). Running multiple batches concurrently means multiple
/// processes, each calling this function single-threaded — the process
/// pool is where concurrency belongs (matches Option A in the companion
/// design doc), not threads inside one call to this function.
///
/// Does not emit build files / run rustfmt postprocessing / refactor
/// passes — those are whole-crate operations that don't make sense for
/// "get per-file pass/fail data for a baseline run" callers, and adding
/// them back is straightforward later (mirror `transpile`'s tail) if a
/// caller needs them.
pub fn transpile_batch_with_results(
    tcfg: &TranspilerConfig,
    cc_db: &Path,
    extra_clang_args: &[&str],
) -> Vec<FileTranspileResult> {
    diagnostics::init(tcfg.enabled_warnings.clone(), tcfg.log_level);

    let build_dir = get_build_dir(tcfg, cc_db);

    let lcmds = get_compile_commands(cc_db, &tcfg.filter).unwrap_or_else(|_| {
        panic!(
            "Could not parse compile commands from {}",
            cc_db.to_string_lossy()
        )
    });

    let clang_args: Vec<String> = get_extra_args_macos();
    let mut clang_args: Vec<&str> = clang_args.iter().map(AsRef::as_ref).collect();
    clang_args.extend_from_slice(extra_clang_args);

    let mut results = Vec::new();

    for lcmd in &lcmds {
        let cmds = &lcmd.cmd_inputs;
        let lcmd_name = lcmd
            .output
            .as_ref()
            .map(|output| {
                let output_path = Path::new(output);
                output_path
                    .file_stem()
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .to_owned()
            })
            .unwrap_or_else(|| tcfg.crate_name());
        let lcmd_build_dir = if lcmd.top_level {
            build_dir.to_path_buf()
        } else {
            build_dir.join(&lcmd_name)
        };

        // Same quadratic common-ancestor computation `transpile` uses —
        // kept identical so build_dir-relative output paths match exactly
        // what the existing single-file-per-process path produces for the
        // same compile_commands.json, which matters for a correctness
        // comparison between the two modes.
        let mut ancestor_path = cmds
            .first()
            .map(|cmd| {
                let mut dir = cmd.abs_file();
                dir.pop();
                dir
            })
            .unwrap_or_else(PathBuf::new);
        if cmds.len() > 1 {
            for cmd in &cmds[1..] {
                let cmd_path = cmd.abs_file();
                ancestor_path = ancestor_path
                    .ancestors()
                    .find(|a| cmd_path.starts_with(a))
                    .map(ToOwned::to_owned)
                    .unwrap_or_else(PathBuf::new);
            }
        }

        for cmd in cmds.iter() {
            let file = cmd.abs_file();
            let mut phase_timings = PhaseTimings::default();

            diagnostics::start_capture();
            // Timed independently of `transpile_single`'s own internal
            // `call_start` (see `PhaseTimings`'s doc comment): a panic
            // partway through the translate phase unwinds before
            // `transpile_single` reaches its own final timing write-back,
            // which would otherwise leave `total_s`/`translate_s` at 0
            // for every panicking file — misleadingly indistinguishable
            // from "the panic happened instantly" when it usually means
            // "already spent most of translate_s's budget before
            // panicking." This outer timer always completes (it wraps
            // `catch_unwind` itself), so `total_s` is always meaningful;
            // only the ast_export_s/translate_s split is unavailable
            // (left at 0) for a panicking file, since that split can only
            // be known by reaching the write-back point inside
            // `transpile_single` that a panic bypassed.
            let call_start = std::time::Instant::now();
            // AssertUnwindSafe: `&mut PhaseTimings` is only not-UnwindSafe
            // because `&mut T` is conservatively assumed to let a panic
            // leave `T` in a torn state a caller could observe — here that
            // "torn state" is at worst a `PhaseTimings` with some fields
            // still zeroed, which is exactly the documented behavior on
            // the `FileTranspileResult::phase_timings` field above, not a
            // soundness hazard. `tcfg`/`clang_args` are shared refs read
            // by `transpile_single`; a panic can't leave read-only data
            // torn.
            //
            // `capture_stderr` wraps the whole `catch_unwind`, not just
            // `transpile_single`'s call: a panicking `f` is fully caught
            // by `catch_unwind` *inside* `capture_stderr`'s closure, so
            // `capture_stderr` itself never sees an unwind reach past it
            // — the fd-restore-on-panic path in `StderrCapture::drop` is
            // therefore only a defense-in-depth backstop (e.g. an abort
            // path this code doesn't otherwise take), and the normal case
            // always returns through `StderrCapture::stop()`, which is
            // what actually reads back Clang's captured diagnostics.
            let (outcome, captured_stderr) = diagnostics::capture_stderr(|| {
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    transpile_single(
                        tcfg,
                        &file,
                        &ancestor_path,
                        &lcmd_build_dir,
                        cc_db,
                        &clang_args,
                        Some(&mut phase_timings),
                    )
                }))
            });
            if outcome.is_err() {
                phase_timings.total_s = call_start.elapsed().as_secs_f64();
            }
            let log_records = diagnostics::take_captured()
                .into_iter()
                .map(CapturedLogRecord::from)
                .collect();

            let (ok, panic_message) = match outcome {
                Ok(Ok(_)) => (true, None),
                Ok(Err(())) => (false, None),
                Err(_payload) => (
                    false,
                    // The panic hook (installed by `diagnostics::init`
                    // above) records the formatted message before this
                    // unwind reaches us; see `take_last_panic_message`'s
                    // doc comment for why this is used instead of
                    // downcasting `_payload` directly.
                    Some(
                        diagnostics::take_last_panic_message()
                            .unwrap_or_else(|| "(panic message not captured)".to_string()),
                    ),
                ),
            };

            results.push(FileTranspileResult {
                file: cmd.file.clone(),
                ok,
                panic_message,
                log_records,
                captured_stderr,
                phase_timings,
            });
        }
    }

    results
}

/// Ensure that clang can locate the system headers on macOS 10.14+.
///
/// MacOS 10.14 does not have a `/usr/include` folder even if Xcode
/// or the command line developer tools are installed as explained in
/// this [thread](https://forums.developer.apple.com/thread/104296).
/// It is possible to install a package which puts the headers in
/// `/usr/include` but the user doesn't have to since we can find
/// the system headers we need by running `xcrun --show-sdk-path`.
fn get_extra_args_macos() -> Vec<String> {
    let mut args = vec![];
    if cfg!(target_os = "macos") {
        let usr_incl = Path::new("/usr/include");
        if !usr_incl.exists() {
            let output = Command::new("xcrun")
                .args(["--show-sdk-path"])
                .output()
                .expect("failed to run `xcrun` subcommand");
            let mut sdk_path = String::from_utf8(output.stdout).unwrap();
            let olen = sdk_path.len();
            sdk_path.truncate(olen - 1);
            sdk_path.push_str("/usr/include");

            args.push("-isystem".to_owned());
            args.push(sdk_path);
        }

        // disable Apple's blocks extension; see https://github.com/immunant/c2rust/issues/229
        args.push("-fno-blocks".to_owned());
    }
    args
}

fn invoke_refactor(build_dir: &Path) -> Result<(), Error> {
    // Make sure the crate builds cleanly
    let status = Command::new("cargo")
        .args(["check"])
        .env("RUSTFLAGS", "-Awarnings")
        .current_dir(build_dir)
        .status()?;
    if !status.success() {
        return Err(failure::format_err!("Crate does not compile."));
    }

    // Assumes the subcommand executable is in the same directory as this program.
    let refactor = env::current_exe()
        .expect("Cannot get current executable path")
        .with_file_name("c2rust-refactor");

    // TODO: we could use `commit` to invoke the refactorer only once
    // with all transforms, but that command is currently broken,
    // see https://github.com/immunant/c2rust/issues/1605.
    for transform in ["rename_unnamed", "reorganize_definitions"] {
        let args = ["--cargo", "--rewrite-mode", "inplace", transform];
        let status = Command::new(&refactor)
            .args(args)
            .current_dir(build_dir)
            .status()
            .map_err(|e| {
                let refactor = refactor.display();
                failure::format_err!("unable to run {refactor}: {e}\nNote that c2rust-refactor must be installed separately from c2rust and c2rust-transpile.")
            })?;
        if !status.success() {
            return Err(failure::format_err!(
                "Refactoring failed. Please fix errors above and re-run:\n    c2rust refactor {}",
                args.join(" "),
            ));
        }
    }

    Ok(())
}

fn reorganize_definitions(
    tcfg: &TranspilerConfig,
    build_dir: &Path,
    crate_file: Option<&Path>,
) -> Result<(), Error> {
    // We only run the reorganization refactoring if we emitted a fresh crate file
    if crate_file.is_none() || tcfg.disable_refactoring || !tcfg.reorganize_definitions {
        return Ok(());
    }

    invoke_refactor(build_dir)?;

    if !tcfg.disable_rustfmt {
        // fix the formatting of the output of `c2rust-refactor`
        let status = Command::new("cargo")
            .args(["fmt"])
            .current_dir(build_dir)
            .status()?;
        if !status.success() {
            warn!("cargo fmt failed, code may not be well-formatted");
        }
    }

    Ok(())
}

/// Invokes `c2rust-postprocess`.
///
/// This assumes the subcommand executable is either in `$PATH`
/// or in the same relative directory as it is in the repo
/// and the current executable is in `target/$profile/`.
fn invoke_postprocess(crate_file: &Path, build_dir: &Path) -> Result<(), Error> {
    let subcommand = "c2rust-postprocess";
    let subcommand_path_buf;
    let subcommand_path = match which(subcommand) {
        Ok(_) => Path::new(subcommand),
        Err(_) => {
            let current_exe = env::current_exe()?;
            let current_exe_dir = current_exe.parent();
            let current_exe = current_exe.display();
            let target_dir = current_exe_dir
                .ok_or_else(|| format_err!("no parent of {current_exe}"))?
                .parent()
                .ok_or_else(|| format_err!("no grandparent of {current_exe}"))?;
            let target_dir_name = target_dir
                .file_name()
                .unwrap_or_default()
                .to_str()
                .unwrap_or_default();
            if target_dir_name != "target" {
                Err(format_err!(
                    "{current_exe} not in `target/$profile/` and {subcommand} not in $PATH"
                ))?;
            }
            let repo_root = target_dir
                .parent()
                .ok_or_else(|| format_err!("no repo root ancestor of {current_exe}"))?;
            subcommand_path_buf = repo_root.join(subcommand).join(subcommand);
            &subcommand_path_buf
        }
    };

    let mut cmd = Command::new(subcommand_path);
    cmd.arg("--update-rust")
        .arg(crate_file)
        .current_dir(build_dir);
    let status =  cmd
        .status()
        .map_err(|e| {
            let path = subcommand_path.display();
            failure::format_err!("unable to run {path}: {e}\nNote that {subcommand} must be installed separately from c2rust and c2rust-transpile.\
            It must be either installed in $PATH or c2rust/c2rust-transpile must be in `target/$profile/` from the c2rust repo.")
        })?;

    if !status.success() {
        Err(format_err!("postprocess failed: {cmd:?}"))?;
    }

    Ok(())
}

fn run_postprocess(
    tcfg: &TranspilerConfig,
    build_dir: &Path,
    crate_file: Option<&Path>,
) -> Result<(), Error> {
    let crate_file = match crate_file {
        Some(crate_file) => crate_file,
        None => return Ok(()),
    };
    if !tcfg.postprocess {
        return Ok(());
    }

    invoke_postprocess(crate_file, build_dir)?;

    Ok(())
}

/// Wall-clock breakdown of one `transpile_single` call, split at the one
/// call site that crosses into C++/Clang (`ast_exporter::get_untyped_ast`)
/// — the natural phase boundary between "Clang parses/exports the AST"
/// and "c2rust's own Rust code converts and translates it". Only
/// populated when a caller passes `Some(&mut _)` into `transpile_single`
/// (the batched path does; the plain `transpile()` loop doesn't need it
/// and skips the `Instant::now()` calls entirely by passing `None`).
#[derive(Debug, Default, Clone, Copy, Serialize)]
pub struct PhaseTimings {
    /// Time inside `ast_exporter::get_untyped_ast` — Clang invocation
    /// (parsing + preprocessing + CBOR AST export), including whatever
    /// time was spent blocked on `ast_exporter::CLANG_MUTEX` waiting for
    /// another thread's call to finish (not a concern for the batch loop,
    /// which is single-threaded per process, but noted since this
    /// function itself has no way to see the wait separately from the
    /// call).
    pub ast_export_s: f64,
    /// Time from just after AST export returns to just before
    /// `transpile_single` returns: typed-AST conversion
    /// (`ConversionContext`), the translator's C-to-Rust conversion, and
    /// the file write + optional rustfmt pass. Not split further because
    /// those steps don't cross an FFI boundary the way AST export does —
    /// they're plain sequential Rust function calls with no natural
    /// "resource acquisition" seam to time separately without much finer
    /// (and noisier) instrumentation.
    pub translate_s: f64,
    /// Total wall-clock for the whole call, timed independently of the
    /// two phases above (not their sum) as a sanity check that no
    /// untimed work sneaks in between/around them.
    pub total_s: f64,
}

/// Transpiles one input C file, writing transpilation output to the filesystem.
#[allow(clippy::too_many_arguments)]
fn transpile_single(
    tcfg: &TranspilerConfig,
    input_path: &Path,
    ancestor_path: &Path,
    build_dir: &Path,
    cc_db: &Path,
    extra_clang_args: &[&str],
    mut phase_timings: Option<&mut PhaseTimings>,
) -> TranspileResult {
    let call_start = phase_timings.is_some().then(std::time::Instant::now);

    let output_path = get_output_path(tcfg, input_path, ancestor_path, build_dir);
    if output_path.exists() && !tcfg.overwrite_existing {
        warn!("Skipping existing file {}", output_path.display());
        return Err(());
    }

    let file = input_path.file_name().unwrap().to_str().unwrap();
    if !input_path.exists() {
        warn!(
            "Input C file {} does not exist, skipping!",
            input_path.display()
        );
        return Err(());
    }

    if tcfg.verbose {
        println!("Additional Clang arguments: {}", extra_clang_args.join(" "));
    }

    // Extract the untyped AST from the CBOR file
    let ast_export_start = phase_timings.is_some().then(std::time::Instant::now);
    let ast_result = ast_exporter::get_untyped_ast(
        input_path,
        cc_db,
        extra_clang_args,
        tcfg.debug_ast_exporter,
        tcfg.emit_c_decl_map,
    );
    let ast_export_s = ast_export_start.map(|t| t.elapsed().as_secs_f64());
    let (untyped_context, preprocessed_source) = match ast_result {
        Err(e) => {
            warn!(
                "Error: {}. Skipping {}; is it well-formed C?",
                e,
                input_path.display()
            );
            if let (Some(pt), Some(ast_export_s), Some(call_start)) =
                (phase_timings.as_deref_mut(), ast_export_s, call_start)
            {
                pt.ast_export_s = ast_export_s;
                pt.total_s = call_start.elapsed().as_secs_f64();
            }
            return Err(());
        }
        Ok(cxt) => cxt,
    };
    let translate_start = phase_timings.is_some().then(std::time::Instant::now);

    // stderr, not stdout: `transpile_batch_with_results` callers (see
    // `c2rust transpile --batch-json`) read stdout as a single JSON
    // document, so nothing on this function's non-debug-dump path may
    // write to stdout or it corrupts that stream. This progress line
    // predates that constraint but nothing parses it (checked: no
    // caller in this repo or linux-rs's scripts/ matches "Transpiling ");
    // stderr is also just the more conventional destination for a
    // progress/status line that isn't the tool's actual output.
    eprintln!("Transpiling {}", file);

    if tcfg.dump_untyped_context {
        println!("CBOR Clang AST");
        println!("{:#?}", untyped_context);
    }

    // Convert this into a typed AST
    let typed_context = {
        let conv = ConversionContext::new(input_path, &untyped_context);
        if conv.invalid_clang_ast && tcfg.fail_on_error {
            panic!("Clang AST was invalid");
        }
        conv.into_typed_context()
    };

    if tcfg.dump_typed_context {
        println!("Clang AST");
        println!("{:#?}", typed_context);
    }

    if tcfg.pretty_typed_context {
        println!("Pretty-printed Clang AST");
        println!("{:#?}", Printer::new(io::stdout()).print(&typed_context));
    }

    // Extract preprocessed text for each function definition so the C decl
    // map can carry it alongside the original source snippet.
    let preprocessed_definitions = preprocessed_source
        .map(|source| {
            translator::collect_preprocessed_definitions(&typed_context, input_path, &source)
        })
        .unwrap_or_default();

    // Perform the translation
    let (translated_string, maybe_decl_map, pragmas, crates) =
        translator::translate(typed_context, tcfg, input_path, &preprocessed_definitions);

    if let Some(decl_map) = maybe_decl_map {
        let decl_map_path = output_path.with_extension("c_decls.json");
        let file = match File::create(&decl_map_path) {
            Ok(file) => file,
            Err(e) => panic!(
                "Unable to open file {} for writing: {}",
                output_path.display(),
                e
            ),
        };

        match serde_json::ser::to_writer(file, &decl_map) {
            Ok(()) => (),
            Err(e) => panic!(
                "Unable to write C declaration map to file {}: {}",
                output_path.display(),
                e
            ),
        };
    }

    let mut file = match File::create(&output_path) {
        Ok(file) => file,
        Err(e) => panic!(
            "Unable to open file {} for writing: {}",
            output_path.display(),
            e
        ),
    };

    match file.write_all(translated_string.as_bytes()) {
        Ok(()) => (),
        Err(e) => panic!(
            "Unable to write translation to file {}: {}",
            output_path.display(),
            e
        ),
    };

    if !tcfg.disable_rustfmt {
        rustfmt(&output_path).edition(tcfg.edition).run();
    }

    if let (Some(pt), Some(ast_export_s), Some(translate_start), Some(call_start)) = (
        phase_timings.as_deref_mut(),
        ast_export_s,
        translate_start,
        call_start,
    ) {
        pt.ast_export_s = ast_export_s;
        pt.translate_s = translate_start.elapsed().as_secs_f64();
        pt.total_s = call_start.elapsed().as_secs_f64();
    }

    Ok((output_path, pragmas, crates))
}

fn get_output_path(
    tcfg: &TranspilerConfig,
    input_path: &Path,
    ancestor_path: &Path,
    build_dir: &Path,
) -> PathBuf {
    // When an output file name is not explicitly specified, we should convert files
    // with dashes to underscores, as they are not allowed in rust file names.
    let file_name = input_path
        .file_name()
        .unwrap()
        .to_str()
        .unwrap()
        .replace('-', "_");

    let mut input_path = input_path.with_file_name(file_name);
    input_path.set_extension("rs");

    if tcfg.output_dir.is_some() {
        let path_buf = input_path
            .strip_prefix(ancestor_path)
            .expect("Couldn't strip common ancestor path");

        // Place the source files in build_dir/src/
        let mut output_path = build_dir.to_path_buf();
        output_path.push("src");
        for elem in path_buf.iter() {
            let path = Path::new(elem);
            let name = get_module_name(path, false, true, false).unwrap();
            output_path.push(name);
        }

        // Create the parent directory if it doesn't exist
        let parent = output_path.parent().unwrap();
        if !parent.exists() {
            fs::create_dir_all(parent).unwrap_or_else(|_| {
                panic!("couldn't create source directory: {}", parent.display())
            });
        }
        output_path
    } else {
        input_path
    }
}
