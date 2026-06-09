use std::io::{self, Write};

/// Print `msg` atomically to stdout via its lock.
pub fn locked_print(msg: &str) {
    let mut out = io::stdout().lock();
    let _ = writeln!(out, "{msg}");
}

/// Print `msg` atomically to stderr via its lock.
pub fn locked_eprint(msg: &str) {
    let mut out = io::stderr().lock();
    let _ = writeln!(out, "{msg}");
}

/// Thread-safe formatted print to stdout.
///
/// ```
/// mgd_common::sync_print!("value = {}", 42);
/// ```
#[macro_export]
macro_rules! sync_print {
    ($($arg:tt)*) => {{
        $crate::output::locked_print(&format!($($arg)*));
    }};
}
