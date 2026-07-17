//! Opt-in registry of kernel-source-idiom rewrites: transformations that
//! recognize a specific, stably-identifiable Linux kernel API (an exact
//! macro decl, or an exact function name + signature) and emit the
//! idiomatic Rust equivalent in place of a literal transliteration of the
//! C implementation.
//!
//! Every rewrite in this module's `KernelIdiomRule` set is off unless
//! explicitly named via `--enable-rule`. With no `--enable-rule` flags,
//! `c2rust transpile` produces the same output as it would with none of
//! these rewrites compiled in at all: a plain, target-idiom-agnostic C-to-
//! Rust transliteration. This matters because c2rust is used standalone by
//! people who have no interest in any particular downstream project's
//! preferred Rust idioms; a rewrite that fired unconditionally would change
//! their output out from under them. Callers who do want a given rewrite
//! (e.g. a project translating Linux kernel sources) opt in per rule.
use std::collections::HashSet;
use std::str::FromStr;

use strum_macros::{Display, EnumString};

/// One independently-toggleable kernel-idiom rewrite.
///
/// Adding a new rewrite means adding a variant here (and handling it at
/// whatever call site recognizes the corresponding macro/function) — no
/// other registration step is needed, since `KernelIdiomRules::is_enabled`
/// already treats `All` as "every variant" without needing to enumerate
/// them.
#[derive(PartialEq, Eq, Hash, Debug, Display, EnumString, Clone, Copy)]
#[strum(serialize_all = "kebab-case")]
pub enum KernelIdiomRule {
    /// Enable every rule below, present and future.
    All,

    /// Recognize expansions of the kernel's `WARN_ON` and `WARN` macros
    /// (matched by macro decl, not by the shape of the expanded
    /// statement-expression) and emit `kernel::warn_on!(condition)` instead
    /// of transliterating the `let __ret = !!(cond); if unlikely(__ret)
    /// {...} unlikely(__ret)` scaffolding either macro expands to.
    WarnOn,

    /// Recognize the kernel's generic bit-scan primitives —
    /// `generic_fls`/`generic___fls`/`generic___ffs`/`fls64`, matched by
    /// exact function name plus parameter/return C type — and emit a
    /// `leading_zeros()`/`trailing_zeros()`-based one-line body instead of
    /// transliterating the header's byte-at-a-time scan loop.
    FlsFamily,

    /// Recognize the expansion of the kernel's `swap(a, b)` macro (matched
    /// by the shape of its `do{}while(0)` body — macro-origin data isn't
    /// available here, since Clang's AST exporter only records it for
    /// expressions, not statements) and emit `core::mem::swap(&mut a, &mut
    /// b)` instead of transliterating the `typeof(a) __tmp = (a); (a) =
    /// (b); (b) = __tmp;` temp-variable dance.
    SwapMemSwap,

    /// Recognize the kernel's `_THIS_IP_` macro (`({ __label__ __here;
    /// __here: (unsigned long)&&__here; })`, GCC's label-as-value
    /// extension used to get an approximate current instruction address
    /// for debug/lockdep annotation — matched by macro-origin, since the
    /// enclosing `StmtExpr` is an `Expr` and carries macro-expansion
    /// provenance the same way `WARN_ON`'s does) and emit a placeholder
    /// value of the expression's own type instead of failing to translate
    /// the unrepresentable `&&label` address-of-label expression at all.
    /// Not a real program-counter capture: every use of `_THIS_IP_` in
    /// this corpus flows only into lockdep-internal debug sinks that are
    /// no-ops when `CONFIG_LOCKDEP` is off, and Rust has no stable
    /// equivalent that fits the `unsigned long` call-site shape
    /// (`core::panic::Location::caller()` is a different type and needs
    /// `#[track_caller]` threaded through call chains that don't have
    /// it) — this preserves the call shape at each `_THIS_IP_` use site
    /// without reimplementing lockdep's own tracking.
    AddrLabelPlaceholder,
}

/// The active set of [`KernelIdiomRule`]s for one transpile run.
#[derive(Debug, Default, Clone)]
pub struct KernelIdiomRules(HashSet<KernelIdiomRule>);

impl KernelIdiomRules {
    /// No rules enabled — literal C transliteration for everything this
    /// module knows how to rewrite, i.e. stock c2rust behavior.
    pub fn none() -> Self {
        Self(HashSet::new())
    }

    /// `rule` is enabled if it was named directly, or if `All` was named.
    pub fn is_enabled(&self, rule: KernelIdiomRule) -> bool {
        self.0.contains(&rule) || self.0.contains(&KernelIdiomRule::All)
    }
}

impl FromIterator<KernelIdiomRule> for KernelIdiomRules {
    fn from_iter<I: IntoIterator<Item = KernelIdiomRule>>(iter: I) -> Self {
        Self(iter.into_iter().collect())
    }
}

/// Every named (non-`All`) rule, for building "known rules" help/error text
/// without hand-maintaining a second list that drifts from the enum itself
/// — see the CLI's `--enable-rule` error message.
pub fn all_named_rules() -> &'static [KernelIdiomRule] {
    &[
        KernelIdiomRule::WarnOn,
        KernelIdiomRule::FlsFamily,
        KernelIdiomRule::SwapMemSwap,
        KernelIdiomRule::AddrLabelPlaceholder,
    ]
}

/// Parse one `--enable-rule` value, which may itself be a comma-separated
/// list (e.g. `--enable-rule=warn-on,fls-family`), matching the CLI's
/// existing convention of accepting either repeated flags or a
/// delimited value for multi-value options.
pub fn parse_rule_list(s: &str) -> Result<Vec<KernelIdiomRule>, String> {
    s.split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(|part| KernelIdiomRule::from_str(part).map_err(|_| format!("unknown rule: {part}")))
        .collect()
}
