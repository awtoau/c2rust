use colored::Colorize;
use failure::{err_msg, Backtrace, Context, Error, Fail};
use fern::colors::ColoredLevelConfig;
use log::{Level, Log, Metadata, Record, SetLoggerError};
use std::cell::RefCell;
use std::collections::HashSet;
use std::fmt::{self, Display};
use std::io;
use std::str::FromStr;
use std::sync::Arc;
use strum_macros::{Display, EnumString};

use crate::c_ast::{ClangAstParseErrorKind, DisplaySrcSpan};
use c2rust_ast_exporter::get_clang_major_version;
use std::io::{Read, Seek};
use std::os::unix::io::AsRawFd;

const DEFAULT_WARNINGS: &[Diagnostic] = &[Diagnostic::ClangAst];

#[derive(PartialEq, Eq, Hash, Debug, Display, EnumString, Clone)]
#[strum(serialize_all = "kebab-case")]
pub enum Diagnostic {
    All,
    Comments,
    ClangAst,
}

macro_rules! diag {
    ($type:path, $($arg:tt)*) => (log::warn!(target: &$type.to_string(), $($arg)*))
}

pub(crate) use diag;

/// One log record captured for the currently-running batch entry: level,
/// target (e.g. a [`Diagnostic`] kebab-case name for `diag!`-emitted
/// warnings, or the module path for plain `log::warn!`/`log::error!`
/// calls), and the formatted message text — the same text a human reading
/// stderr would see, and the same text `run_c2rust_baseline.py`'s
/// `extract_signatures()` regex-matches out of raw stderr today. Kept as
/// structured `(level, target, message)` instead of a pre-formatted line
/// so callers can pattern-match on `target`/`message` directly instead of
/// re-parsing fern's coloring/prefix formatting.
#[derive(Debug, Clone)]
pub struct CapturedRecord {
    pub level: Level,
    pub target: String,
    pub message: String,
}

thread_local! {
    // Some(_) while a batch caller has asked to capture this thread's log
    // output for one file; None (the default) means "just forward to the
    // normal stderr logger", which is what the plain single-file CLI path
    // (transpile()/transpile_single() called once, no capture requested)
    // always sees — capture is strictly additive.
    static CAPTURE: RefCell<Option<Vec<CapturedRecord>>> = const { RefCell::new(None) };
}

/// Start capturing this thread's log records instead of (well, in addition
/// to — see [`CaptureLog`]) sending them to stderr. Call once per file
/// immediately before invoking the per-file transpile step; pair with
/// [`take_captured`] immediately after to read back what that one call
/// logged. Thread-local because the batch loop this exists for
/// (`transpile_batch_with_results`) is intentionally single-threaded (see
/// its doc comment for why), so "this thread's log output between start
/// and take" unambiguously means "this file's log output".
pub fn start_capture() {
    CAPTURE.with(|c| *c.borrow_mut() = Some(Vec::new()));
}

/// Stop capturing and return everything captured since the matching
/// [`start_capture`] call.
pub fn take_captured() -> Vec<CapturedRecord> {
    CAPTURE.with(|c| c.borrow_mut().take().unwrap_or_default())
}

thread_local! {
    // Last panic message this thread's panic hook observed, formatted
    // exactly like the default hook's stderr output ("'<msg>', <file>:
    // <line>:<col>"). Read via `take_last_panic_message` immediately
    // after a `catch_unwind` that returned `Err`. Deliberately NOT
    // cleared automatically on catch — `take_last_panic_message` clears
    // it — so a caller that forgets to check still sees stale-but-real
    // data on the next panic rather than a silent gap; two panics in a
    // row (impossible within one file's single `catch_unwind` here, but
    // not a hazard the hook itself needs to prevent) would just overwrite
    // with the newer message, matching how the batch loop calls this
    // once per file.
    static LAST_PANIC_MESSAGE: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// Read back this thread's `panic!` payload as a display string instead of
/// downcasting `catch_unwind`'s `Box<dyn Any + Send>` payload directly.
/// This project's actual panic sites (`.expect("literal")`,
/// `indexmap`'s internal `.expect(...)` panics, etc.) were found via
/// direct testing to NOT reliably downcast to `&str`/`String` in this
/// pinned nightly-2022-11-03 toolchain in every call context (verified:
/// an identical `.expect("...")` pattern downcasts fine as a standalone
/// unit test in this same crate, but the *same* two real in-corpus panic
/// sites — one in `translator/mod.rs`, one inside `indexmap`'s
/// `Index::index` — both produced a `Box<dyn Any>` whose `TypeId` matched
/// neither `&str` nor `String` when caught from inside
/// `transpile_batch_with_results`'s call chain; root cause not
/// conclusively identified, plausibly an old-nightly panic-payload
/// boxing quirk specific to that deeper/hotter call path). Capturing the
/// hook's pre-formatted message sidesteps the payload type entirely —
/// it's the same string a human reading stderr already relies on, and
/// what today's stderr-parsing baseline harness regexes out via
/// `PANIC_RE`, so it's provably sufficient for this project's purposes
/// even though it's coarser than a typed payload would be.
pub fn take_last_panic_message() -> Option<String> {
    LAST_PANIC_MESSAGE.with(|c| c.borrow_mut().take())
}

/// Install a panic hook that records the formatted panic message into
/// `LAST_PANIC_MESSAGE` before delegating to whatever hook was previously
/// installed (so `RUST_BACKTRACE=1` output and the plain CLI's existing
/// stderr panic banner are unaffected — this hook is additive, same
/// principle as `CaptureLog` above). Idempotent-safe to call more than
/// once via `std::sync::Once`: `transpile_batch_with_results` calls
/// `diagnostics::init` (which calls this) once per `compile_commands.json`
/// batch, and installing a second wrapping hook on a later batch would
/// otherwise nest hooks arbitrarily deep across a long-running process
/// that processes many batches sequentially (e.g. a driver script calling
/// this library function once per PCH-eligible flag-set group).
fn install_panic_message_hook() {
    static INSTALLED: std::sync::Once = std::sync::Once::new();
    INSTALLED.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            LAST_PANIC_MESSAGE.with(|c| {
                *c.borrow_mut() = Some(info.to_string());
            });
            previous(info);
        }));
    });
}

/// Redirects the process's real fd 2 (stderr) to a temp file for the
/// lifetime of this guard, restoring the original fd 2 on drop (including
/// on unwind — `Drop::drop` still runs during a panic unless the panic
/// happens while already unwinding from another panic, which cannot occur
/// within one file's single `catch_unwind` call in the batch loop this
/// exists for).
///
/// This exists because a meaningful share of what a caller needs (Clang's
/// own diagnostics — e.g. `printDiag()` calls in `AstExporter.cpp` for
/// "Cannot translate GNU address of label expression" and friends) are
/// emitted by the C++ `DiagnosticsEngine` via `llvm::errs()` during the
/// `ast_exporter()` FFI call, which is a raw OS-level write to fd 2 that
/// completely bypasses Rust's `log` crate — so `CaptureLog` below, which
/// only sees records that go through `log`, cannot see them no matter
/// what logger is installed. There is no supported hook into
/// `DiagnosticsEngine`'s consumer from the Rust side without new
/// AstExporter.cpp engineering (out of scope here — this project's
/// existing per-process isolation already gets this data "for free" by
/// having a whole OS process per file whose stderr the Python harness
/// reads directly; this guard reproduces the same fd-level capture
/// in-process, one file's worth at a time, instead).
///
/// Uses a real temp file rather than a pipe: a pipe has a bounded kernel
/// buffer (`ast_exporter` output for a large kernel header stack can run
/// to hundreds of lines) and nothing is reading the other end
/// concurrently while `ast_exporter()` runs, so a sufficiently chatty
/// file would deadlock the writer once the pipe filled; a file has no
/// such bound.
struct StderrCapture {
    saved_fd: std::os::unix::io::RawFd,
    tmp: std::fs::File,
}

impl StderrCapture {
    fn start() -> io::Result<Self> {
        let tmp = tempfile::tempfile()?;
        // SAFETY: `dup`/`dup2` are called with fds this process owns
        // (`STDERR_FILENO` is always valid for a running process; `tmp`'s
        // fd is valid because we just created it and hold it live in
        // `self.tmp` for the guard's lifetime). No other thread is
        // expected to write to fd 2 concurrently — the batch loop this
        // guard is used from is documented single-threaded specifically
        // because `ast_exporter`'s own `CLANG_MUTEX` already forces that
        // (see `transpile_batch_with_results`'s doc comment) — so there
        // is no cross-thread race on the process-global fd table here.
        let saved_fd = unsafe { libc::dup(libc::STDERR_FILENO) };
        if saved_fd < 0 {
            return Err(io::Error::last_os_error());
        }
        if unsafe { libc::dup2(tmp.as_raw_fd(), libc::STDERR_FILENO) } < 0 {
            let err = io::Error::last_os_error();
            unsafe { libc::close(saved_fd) };
            return Err(err);
        }
        Ok(Self { saved_fd, tmp })
    }

    /// Restore the real fd 2 and return everything written to it while
    /// captured, as text (invalid UTF-8, if any slips in from a stray
    /// non-UTF8 path in a diagnostic, is replaced rather than causing
    /// this to fail — same lossy-decode policy the Python harness's
    /// `subprocess.Popen(..., text=True)` already applies to a
    /// per-process run's stderr).
    fn stop(mut self) -> String {
        // Flush stdio's own C-level buffering for fd 2 before restoring
        // it, or buffered-but-not-yet-written bytes from this capture
        // window could land after dup2 restores the real fd, corrupting
        // whichever later capture window (or the real terminal) happens
        // to be active when libc finally flushes them.
        unsafe { libc::fflush(std::ptr::null_mut()) };
        unsafe { libc::dup2(self.saved_fd, libc::STDERR_FILENO) };
        unsafe { libc::close(self.saved_fd) };
        let mut buf = Vec::new();
        if self.tmp.rewind().is_ok() {
            let _ = self.tmp.read_to_end(&mut buf);
        }
        String::from_utf8_lossy(&buf).into_owned()
    }
}

impl Drop for StderrCapture {
    fn drop(&mut self) {
        // Best-effort restore if `stop()` was never called (e.g. a
        // double-panic scenario) — without this, a leaked redirect would
        // leave every subsequent file's Clang diagnostics silently
        // vanishing into an unread temp file instead of surfacing
        // anywhere, which is a much worse failure mode than a slightly
        // redundant restore here.
        unsafe {
            libc::fflush(std::ptr::null_mut());
            libc::dup2(self.saved_fd, libc::STDERR_FILENO);
            libc::close(self.saved_fd);
        }
    }
}

/// Redirect fd 2 to a temp file for the duration of `f`, returning both
/// `f`'s result and everything written to real stderr (by any means —
/// Rust's `log`/`eprintln!`, or C++ writing directly via `llvm::errs()`)
/// while it ran. `f` itself may panic; the fd is still restored (via
/// `StderrCapture`'s `Drop` impl) before the unwind continues past this
/// function, so a panicking file never leaves stderr redirected for the
/// next file in a batch.
pub fn capture_stderr<F: FnOnce() -> R, R>(f: F) -> (R, String) {
    // If the redirect itself can't be set up (fd exhaustion or similar),
    // fail loudly rather than silently running `f` without capture and
    // returning an empty string that looks like "this file produced no
    // Clang diagnostics" — that would be indistinguishable from a
    // genuinely clean file and could hide real warnings from a caller.
    let guard = StderrCapture::start().expect("failed to redirect stderr for capture");
    let result = f();
    let captured = guard.stop();
    (result, captured)
}

/// Wraps the real stderr-formatting logger and additionally appends every
/// record to the thread-local capture buffer, if one is active. This
/// means capture is a pure add-on: with no `start_capture()` call, output
/// is byte-for-byte what it always was (the CLI's plain
/// `c2rust transpile <cc.json>` path never calls `start_capture`, so its
/// stderr is unaffected by this module existing).
struct CaptureLog {
    inner: Box<dyn Log>,
}

impl Log for CaptureLog {
    fn enabled(&self, metadata: &Metadata) -> bool {
        self.inner.enabled(metadata)
    }

    fn log(&self, record: &Record) {
        // Mirror fern's own enabled-check: Log::log() is called
        // unconditionally by the log facade macros (they only consult
        // `log::max_level()`, a coarse global cutoff), so without this
        // check every below-cutoff-but-still-max_level record would land
        // in the capture buffer even though it would never have reached
        // stderr.
        if self.inner.enabled(record.metadata()) {
            CAPTURE.with(|c| {
                if let Some(buf) = c.borrow_mut().as_mut() {
                    buf.push(CapturedRecord {
                        level: record.level(),
                        target: record.target().to_string(),
                        message: record.args().to_string(),
                    });
                }
            });
        }
        self.inner.log(record);
    }

    fn flush(&self) {
        self.inner.flush();
    }
}

pub fn init(mut enabled_warnings: HashSet<Diagnostic>, log_level: log::LevelFilter) {
    install_panic_message_hook();
    enabled_warnings.extend(DEFAULT_WARNINGS.iter().cloned());

    let colors = ColoredLevelConfig::new();
    let (max_level, logger) = fern::Dispatch::new()
        .format(move |out, message, record| {
            let level_label = match record.level() {
                Level::Error => "error",
                Level::Warn => "warning",
                Level::Info => "info",
                Level::Debug => "debug",
                Level::Trace => "trace",
            };
            let target = record.target();
            let warn_flag = Diagnostic::from_str(target)
                .map(|_| format!(" [-W{}]", target))
                .unwrap_or_default();
            out.finish(format_args!(
                "\x1B[{}m{}:\x1B[0m {}{}",
                colors.get_color(&record.level()).to_fg_str(),
                level_label,
                message,
                warn_flag,
            ))
        })
        .level(log_level)
        .filter(move |metadata| {
            if enabled_warnings.contains(&Diagnostic::All) {
                return true;
            }
            Diagnostic::from_str(metadata.target())
                .map(|d| enabled_warnings.contains(&d))
                .unwrap_or(true)
        })
        .chain(io::stderr())
        .into_log();
    let logger: Box<dyn Log> = Box::new(CaptureLog { inner: logger });
    // Ignore the [`SetLoggerError`] b/c we just want to make sure it's set at least once.
    let _: Result<(), SetLoggerError> = log_reroute::init();
    log_reroute::reroute_boxed(logger);
    log::set_max_level(max_level);
}

#[derive(Debug, Clone)]
pub struct TranslationError {
    loc: Vec<DisplaySrcSpan>,
    inner: Arc<Context<TranslationErrorKind>>,
}

pub type TranslationResult<T> = Result<T, TranslationError>;

#[derive(Clone, Eq, PartialEq, Debug)]
pub enum TranslationErrorKind {
    Generic,

    // Not enough simd intrinsics are available in LLVM < 7
    OldLLVMSimd,

    // We are waiting for va_copy support to land in rustc
    VaCopyNotImplemented,

    // Clang AST exported by AST-exporter was not valid
    InvalidClangAst(ClangAstParseErrorKind),
}

/// Constructs a `TranslationError` using the standard string interpolation syntax.
#[macro_export]
macro_rules! format_translation_err {
    ($loc:expr, $($arg:tt)*) => {
        TranslationError::new(
            $loc,
            failure::err_msg(format!($($arg)*))
                .context(TranslationErrorKind::Generic),
        )
    }
}

impl Display for TranslationErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        use self::TranslationErrorKind::*;
        match self {
            Generic => {}

            OldLLVMSimd => {
                if let Some(version) = get_clang_major_version() {
                    if version < 7 {
                        return write!(f, "SIMD intrinsics require LLVM 7 or newer. Please build C2Rust against a newer LLVM version.");
                    }
                }
            }

            VaCopyNotImplemented => {
                return write!(f, "Rust does not yet support a C-compatible va_copy which is required to translate this function. See https://github.com/rust-lang/rust/pull/59625");
            }

            InvalidClangAst(_) => {
                return write!(f, "Exported Clang AST was invalid. Check warnings above for unimplemented features.");
            }
        }
        Ok(())
    }
}

impl Fail for TranslationError {
    fn cause(&self) -> Option<&dyn Fail> {
        self.inner.cause()
    }

    fn backtrace(&self) -> Option<&Backtrace> {
        self.inner.backtrace()
    }
}

impl Display for TranslationError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        if let Some(cause) = self.cause() {
            writeln!(f, "{}", cause)?;
        }
        match self.inner.get_context() {
            TranslationErrorKind::Generic => {}
            ref kind => writeln!(f, "{}", kind)?,
        }
        for loc in &self.loc {
            writeln!(f, "{} {}", "-->".blue(), loc)?;
        }
        Ok(())
    }
}

impl TranslationError {
    pub fn kind(&self) -> TranslationErrorKind {
        self.inner.get_context().clone()
    }

    pub fn new(loc: Option<DisplaySrcSpan>, inner: Context<TranslationErrorKind>) -> Self {
        Self::from(inner).add_loc(loc)
    }

    pub fn generic(msg: &'static str) -> Self {
        msg.into()
    }

    pub fn add_loc(mut self, loc: Option<DisplaySrcSpan>) -> Self {
        if let Some(loc) = loc {
            self.loc.push(loc);
        }
        self
    }
}

impl From<&'static str> for TranslationError {
    fn from(msg: &'static str) -> Self {
        err_msg(msg).context(TranslationErrorKind::Generic).into()
    }
}

impl From<Error> for TranslationError {
    fn from(e: Error) -> Self {
        e.context(TranslationErrorKind::Generic).into()
    }
}

impl From<TranslationErrorKind> for TranslationError {
    fn from(kind: TranslationErrorKind) -> Self {
        Context::new(kind).into()
    }
}

impl From<Context<TranslationErrorKind>> for TranslationError {
    fn from(ctx: Context<TranslationErrorKind>) -> Self {
        Self {
            loc: Vec::new(),
            inner: Arc::new(ctx),
        }
    }
}
