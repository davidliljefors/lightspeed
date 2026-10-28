//! Diagnostic output on stderr (the editor shows it in the server's output panel).
//! Set `LIGHTSPEED_QUIET` to silence it.

use std::sync::OnceLock;

pub fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("LIGHTSPEED_QUIET").is_none())
}

/// `eprintln!` unless `LIGHTSPEED_QUIET` is set.
#[macro_export]
macro_rules! log {
    ($($arg:tt)*) => {
        if $crate::base::log::enabled() {
            eprintln!($($arg)*);
        }
    };
}
