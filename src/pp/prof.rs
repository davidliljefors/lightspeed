//! Phase profiler for the preprocessor, compiled in with the `profile` cargo
//! feature. Time is charged to exactly one phase at a time (the innermost
//! `enter`), measured with the TSC and flushed to process-wide totals after
//! every translation unit. Without the feature every method is an empty inline
//! function.

#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(usize)]
pub(super) enum Phase {
    /// Per-TU reset of the macro table and state.
    Setup,
    /// Lexing the main file.
    MainLex,
    /// Main-file token stream: expansion and emission.
    Expand,
    /// Directive dispatch and header scanning (excluding the phases below).
    Scan,
    /// `enter_file` bookkeeping: guard/once checks, header cache.
    EnterFile,
    /// Include search bookkeeping, excluding probes.
    Resolve,
    /// Filesystem existence probes.
    Probe,
    /// Reading a header from disk.
    Load,
    /// Lexing and compacting a header.
    Lex,
    Define,
    /// `#if`/`#elif` evaluation, including macro expansion in the condition.
    Cond,
    /// Skipping inactive groups.
    Skip,
    /// Dependency sort and diagnostics at the end of a TU.
    Tail,
}

#[cfg(feature = "profile")]
pub(super) const PHASE_NAMES: [&str; 13] =
    ["setup", "main_lex", "expand", "scan", "enter_file", "resolve", "probe", "load", "lex", "define", "cond", "skip", "tail"];

#[derive(Clone, Copy)]
#[repr(usize)]
pub(super) enum Count {
    Runs,
    Directives,
    Defines,
    Conds,
    Resolves,
    ResolveHits,
    Probes,
    HeaderLoads,
    HeaderBytes,
    HeaderTokens,
    SkippedTokens,
    GuardSkips,
    OnceSkips,
    Entered,
    MainTokens,
}

#[cfg(feature = "profile")]
pub(super) const COUNT_NAMES: [&str; 15] = [
    "runs", "directives", "defines", "conds", "resolves", "resolve_hits", "probes", "header_loads", "header_bytes", "header_tokens", "skipped_tokens",
    "guard_skips", "once_skips", "headers_entered", "main_tokens",
];

#[cfg(feature = "profile")]
mod on {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

    const NP: usize = PHASE_NAMES.len();
    const NC: usize = COUNT_NAMES.len();
    static CYC: [AtomicU64; NP] = [const { AtomicU64::new(0) }; NP];
    static CNT: [AtomicU64; NC] = [const { AtomicU64::new(0) }; NC];
    static CLOCK: std::sync::OnceLock<(std::time::Instant, u64)> = std::sync::OnceLock::new();

    fn tsc() -> u64 {
        // SAFETY: rdtsc has no preconditions.
        unsafe { core::arch::x86_64::_rdtsc() }
    }

    pub(in crate::pp) struct Prof {
        cur: Phase,
        last: u64,
        cyc: [u64; NP],
        cnt: [u64; NC],
    }

    impl Prof {
        pub fn new() -> Self {
            CLOCK.get_or_init(|| (std::time::Instant::now(), tsc()));
            Self { cur: Phase::Setup, last: tsc(), cyc: [0; NP], cnt: [0; NC] }
        }

        pub fn begin(&mut self) {
            self.cur = Phase::Setup;
            self.last = tsc();
        }

        pub fn enter(&mut self, p: Phase) -> Phase {
            let now = tsc();
            self.cyc[self.cur as usize] += now - self.last;
            self.last = now;
            std::mem::replace(&mut self.cur, p)
        }

        pub fn leave(&mut self, prev: Phase) {
            self.enter(prev);
        }

        pub fn count(&mut self, c: Count, n: u64) {
            self.cnt[c as usize] += n;
        }

        pub fn flush(&mut self) {
            self.enter(Phase::Setup);
            for (g, l) in CYC.iter().zip(self.cyc.iter_mut()) {
                g.fetch_add(std::mem::take(l), Relaxed);
            }
            for (g, l) in CNT.iter().zip(self.cnt.iter_mut()) {
                g.fetch_add(std::mem::take(l), Relaxed);
            }
        }
    }

    pub fn report() -> String {
        let Some(&(t0, c0)) = CLOCK.get() else { return String::new() };
        let ns_per_cycle = t0.elapsed().as_nanos() as f64 / (tsc() - c0).max(1) as f64;
        let ms = |c: u64| c as f64 * ns_per_cycle / 1e6;
        let total: u64 = CYC.iter().map(|c| c.load(Relaxed)).sum();
        let runs = CNT[Count::Runs as usize].load(Relaxed).max(1);
        let mut s = format!("preprocessor profile: {} TUs, {:.0} ms total CPU, {:.1} us/TU\n", runs, ms(total), ms(total) * 1e3 / runs as f64);
        for (n, c) in PHASE_NAMES.iter().zip(CYC.iter()) {
            let c = c.load(Relaxed);
            s.push_str(&format!("  {n:<11} {:>9.1} ms {:>5.1}%  {:>8.1} us/TU\n", ms(c), c as f64 * 100.0 / total.max(1) as f64, ms(c) * 1e3 / runs as f64));
        }
        for (n, c) in COUNT_NAMES.iter().zip(CNT.iter()) {
            let c = c.load(Relaxed);
            s.push_str(&format!("  {n:<15} {c:>12} ({:.1}/TU)\n", c as f64 / runs as f64));
        }
        s
    }
}

#[cfg(feature = "profile")]
pub(super) use on::Prof;
#[cfg(feature = "profile")]
pub use on::report;

#[cfg(not(feature = "profile"))]
mod off {
    use super::*;

    pub(in crate::pp) struct Prof;

    impl Prof {
        #[inline(always)]
        pub fn new() -> Self {
            Self
        }
        #[inline(always)]
        pub fn begin(&mut self) {}
        #[inline(always)]
        pub fn enter(&mut self, _: Phase) -> Phase {
            Phase::Setup
        }
        #[inline(always)]
        pub fn leave(&mut self, _: Phase) {}
        #[inline(always)]
        pub fn count(&mut self, _: Count, _: u64) {}
        #[inline(always)]
        pub fn flush(&mut self) {}
    }

    pub fn report() -> String {
        String::new()
    }
}

#[cfg(not(feature = "profile"))]
pub(super) use off::Prof;
#[cfg(not(feature = "profile"))]
pub use off::report;
